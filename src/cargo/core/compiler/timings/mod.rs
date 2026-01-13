//! Timing tracking.
//!
//! This module implements some simple tracking information for timing of how
//! long it takes for different units to compile.

pub mod report;

use super::CompileMode;
use super::Unit;
use super::UnitIndex;
use crate::core::PackageId;
use crate::core::compiler::BuildContext;
use crate::core::compiler::BuildRunner;
use crate::core::compiler::job_queue::JobId;
use crate::ops::cargo_report::timings::prepare_context;
use crate::util::cpu::State;
use crate::util::log_message::LogMessage;
use crate::util::style;
use crate::util::{CargoResult, GlobalContext};

use cargo_util::paths;
use std::collections::HashMap;
use std::io::BufWriter;
use std::time::{Duration, Instant};

/// Tracking information for the entire build.
///
/// Methods on this structure are generally called from the main thread of a
/// running [`JobQueue`] instance (`DrainState` in specific) when the queue
/// receives messages from spawned off threads.
///
/// [`JobQueue`]: super::JobQueue
pub struct Timings<'gctx> {
    gctx: &'gctx GlobalContext,
    /// Whether or not timings should be captured.
    enabled: bool,
    /// When Cargo started.
    start: Instant,
    /// A rendered string of when compilation started.
    start_str: String,
    /// A summary of the root units.
    ///
    /// Total number of fresh units.
    total_fresh: u32,
    /// Total number of dirty units.
    total_dirty: u32,
    /// A map from unit to index.
    unit_to_index: HashMap<Unit, UnitIndex>,
    /// Time tracking for each individual unit.
    unit_times: Vec<UnitTime>,
    /// Units that are in the process of being built.
    /// When they finished, they are moved to `unit_times`.
    active: HashMap<JobId, UnitTime>,
    /// Last recorded state of the system's CPUs and when it happened
    last_cpu_state: Option<State>,
    last_cpu_recording: Instant,
    /// Recorded CPU states, stored as tuples. First element is when the
    /// recording was taken and second element is percentage usage of the
    /// system.
    cpu_usage: Vec<(f64, f64)>,
}

/// Section of compilation (e.g. frontend, backend, linking).
#[derive(Copy, Clone, serde::Serialize)]
pub struct CompilationSection {
    /// Start of the section, as an offset in seconds from `UnitTime::start`.
    pub start: f64,
    /// End of the section, as an offset in seconds from `UnitTime::start`.
    pub end: Option<f64>,
}

/// Tracking information for an individual unit.
struct UnitTime {
    unit: Unit,
    /// The time when this unit started as an offset in seconds from `Timings::start`.
    start: f64,
    /// Total time to build this unit in seconds.
    duration: f64,
    /// The time when the `.rmeta` file was generated, an offset in seconds
    /// from `start`.
    rmeta_time: Option<f64>,
    /// Reverse deps that are unblocked and ready to run after this unit finishes.
    unblocked_units: Vec<Unit>,
    /// Same as `unblocked_units`, but unblocked by rmeta.
    unblocked_rmeta_units: Vec<Unit>,
}

/// Data for a single compilation unit, prepared for serialization to JSON.
///
/// This is used by the HTML report's JavaScript to render the pipeline graph.
#[derive(serde::Serialize)]
pub struct UnitData {
    pub i: UnitIndex,
    pub name: String,
    pub version: String,
    pub mode: String,
    pub target: String,
    pub features: Vec<String>,
    pub start: f64,
    pub duration: f64,
    pub unblocked_units: Vec<UnitIndex>,
    pub unblocked_rmeta_units: Vec<UnitIndex>,
    pub sections: Option<Vec<(report::SectionName, report::SectionData)>>,
}

impl<'gctx> Timings<'gctx> {
    pub fn new(bcx: &BuildContext<'_, 'gctx>, root_units: &[Unit]) -> Timings<'gctx> {
        let start = bcx.gctx.creation_time();
        let enabled = bcx.logger.is_some();

        if !enabled {
            return Timings {
                gctx: bcx.gctx,
                enabled,
                start,
                start_str: String::new(),
                total_fresh: 0,
                total_dirty: 0,
                unit_to_index: HashMap::new(),
                unit_times: Vec::new(),
                active: HashMap::new(),
                last_cpu_state: None,
                last_cpu_recording: Instant::now(),
                cpu_usage: Vec::new(),
            };
        }

        let mut root_map: HashMap<PackageId, Vec<String>> = HashMap::new();
        for unit in root_units {
            let target_desc = unit.target.description_named();
            root_map
                .entry(unit.pkg.package_id())
                .or_default()
                .push(target_desc);
        }
        let start_str = jiff::Timestamp::now().to_string();
        let last_cpu_state = match State::current() {
            Ok(state) => Some(state),
            Err(e) => {
                tracing::info!("failed to get CPU state, CPU tracking disabled: {:?}", e);
                None
            }
        };

        Timings {
            gctx: bcx.gctx,
            enabled,
            start,
            start_str,
            total_fresh: 0,
            total_dirty: 0,
            unit_to_index: bcx.unit_to_index.clone(),
            unit_times: Vec::new(),
            active: HashMap::new(),
            last_cpu_state,
            last_cpu_recording: Instant::now(),
            cpu_usage: Vec::new(),
        }
    }

    /// Mark that a unit has started running.
    pub fn unit_start(&mut self, build_runner: &BuildRunner<'_, '_>, id: JobId, unit: Unit) {
        let Some(logger) = build_runner.bcx.logger else {
            return;
        };
        let mut target = if unit.target.is_lib()
            && matches!(unit.mode, CompileMode::Build | CompileMode::Check { .. })
        {
            // Special case for brevity, since most dependencies hit this path.
            "".to_string()
        } else {
            format!(" {}", unit.target.description_named())
        };
        match unit.mode {
            CompileMode::Test => target.push_str(" (test)"),
            CompileMode::Build => {}
            CompileMode::Check { test: true } => target.push_str(" (check-test)"),
            CompileMode::Check { test: false } => target.push_str(" (check)"),
            CompileMode::Doc { .. } => target.push_str(" (doc)"),
            CompileMode::Doctest => target.push_str(" (doc test)"),
            CompileMode::Docscrape => target.push_str(" (doc scrape)"),
            CompileMode::RunCustomBuild => target.push_str(" (run)"),
        }
        let start = self.start.elapsed().as_secs_f64();
        let unit_time = UnitTime {
            unit,
            start,
            duration: 0.0,
            rmeta_time: None,
            unblocked_units: Vec::new(),
            unblocked_rmeta_units: Vec::new(),
        };
        logger.log(LogMessage::UnitStarted {
            index: self.unit_to_index[&unit_time.unit],
            elapsed: start,
        });
        assert!(self.active.insert(id, unit_time).is_none());
    }

    /// Mark that the `.rmeta` file as generated.
    pub fn unit_rmeta_finished(
        &mut self,
        build_runner: &BuildRunner<'_, '_>,
        id: JobId,
        unblocked: Vec<&Unit>,
    ) {
        let Some(logger) = build_runner.bcx.logger else {
            return;
        };
        // `id` may not always be active. "fresh" units unconditionally
        // generate `Message::Finish`, but this active map only tracks dirty
        // units.
        let Some(unit_time) = self.active.get_mut(&id) else {
            return;
        };
        let elapsed = self.start.elapsed().as_secs_f64();
        unit_time.rmeta_time = Some(elapsed - unit_time.start);
        assert!(unit_time.unblocked_rmeta_units.is_empty());
        unit_time
            .unblocked_rmeta_units
            .extend(unblocked.iter().cloned().cloned());

        let unblocked = unblocked.iter().map(|u| self.unit_to_index[u]).collect();
        logger.log(LogMessage::UnitRmetaFinished {
            index: self.unit_to_index[&unit_time.unit],
            elapsed,
            unblocked,
        });
    }

    /// Mark that a unit has finished running.
    pub fn unit_finished(
        &mut self,
        build_runner: &BuildRunner<'_, '_>,
        id: JobId,
        unblocked: Vec<&Unit>,
    ) {
        let Some(logger) = build_runner.bcx.logger else {
            return;
        };
        // See note above in `unit_rmeta_finished`, this may not always be active.
        let Some(mut unit_time) = self.active.remove(&id) else {
            return;
        };
        let elapsed = self.start.elapsed().as_secs_f64();
        unit_time.duration = elapsed - unit_time.start;
        assert!(unit_time.unblocked_units.is_empty());
        unit_time
            .unblocked_units
            .extend(unblocked.iter().cloned().cloned());

        let unblocked = unblocked.iter().map(|u| self.unit_to_index[u]).collect();
        logger.log(LogMessage::UnitFinished {
            index: self.unit_to_index[&unit_time.unit],
            elapsed,
            unblocked,
        });
        self.unit_times.push(unit_time);
    }

    /// Handle the start/end of a compilation section.
    pub fn unit_section_timing(
        &mut self,
        build_runner: &BuildRunner<'_, '_>,
        id: JobId,
        section_timing: &SectionTiming,
    ) {
        let Some(logger) = build_runner.bcx.logger else {
            return;
        };
        let Some(unit_time) = self.active.get_mut(&id) else {
            return;
        };
        let elapsed = self.start.elapsed().as_secs_f64();

        let index = self.unit_to_index[&unit_time.unit];
        let section = section_timing.name.clone();
        logger.log(match section_timing.event {
            SectionTimingEvent::Start => LogMessage::UnitSectionStarted {
                index,
                elapsed,
                section,
            },
            SectionTimingEvent::End => LogMessage::UnitSectionFinished {
                index,
                elapsed,
                section,
            },
        })
    }

    /// Mark that a fresh unit was encountered. (No re-compile needed)
    pub fn add_fresh(&mut self) {
        self.total_fresh += 1;
    }

    /// Mark that a dirty unit was encountered. (Re-compile needed)
    pub fn add_dirty(&mut self) {
        self.total_dirty += 1;
    }

    /// Take a sample of CPU usage
    pub fn record_cpu(&mut self) {
        if !self.enabled {
            return;
        }
        let Some(prev) = &mut self.last_cpu_state else {
            return;
        };
        // Don't take samples too frequently, even if requested.
        let now = Instant::now();
        if self.last_cpu_recording.elapsed() < Duration::from_millis(100) {
            return;
        }
        let current = match State::current() {
            Ok(s) => s,
            Err(e) => {
                tracing::info!("failed to get CPU state: {:?}", e);
                return;
            }
        };
        let pct_idle = current.idle_since(prev);
        *prev = current;
        self.last_cpu_recording = now;
        let dur = now.duration_since(self.start).as_secs_f64();
        self.cpu_usage.push((dur, 100.0 - pct_idle));
    }

    /// Call this when all units are finished.
    pub fn finished(&mut self, build_runner: &BuildRunner<'_, '_>) -> CargoResult<()> {
        if let Some(logger) = build_runner.bcx.logger
            && let Some(logs) = logger.get_logs() {
            let timestamp = self.start_str.replace(&['-', ':'][..], "");
            let timings_path = build_runner
                .files()
                .timings_dir()
                .expect("artifact-dir was not locked");
            paths::create_dir_all(&timings_path)?;
            let filename = timings_path.join(format!("cargo-timing-{}.html", timestamp));
            let mut f = BufWriter::new(paths::create(&filename)?);

            let run_id = logger.run_id();
            let ctx = prepare_context(logs.into_iter(), run_id)?;
            report::write_html(ctx, &mut f)?;

            let unstamped_filename = timings_path.join("cargo-timing.html");
            paths::link_or_copy(&filename, &unstamped_filename)?;

            let mut shell = self.gctx.shell();
            let timing_path = std::env::current_dir().unwrap_or_default().join(&filename);
            let link = shell.err_file_hyperlink(&timing_path);
            let msg = format!("report saved to {link}{}{link:#}", timing_path.display(),);
            shell.status_with_color("Timing", msg, &style::NOTE)?;
        }
        Ok(())
    }
}

/// Start or end of a section timing.
#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum SectionTimingEvent {
    Start,
    End,
}

/// Represents a certain section (phase) of rustc compilation.
/// It is emitted by rustc when the `--json=timings` flag is used.
#[derive(serde::Deserialize, Debug)]
pub struct SectionTiming {
    pub name: String,
    pub event: SectionTimingEvent,
}
