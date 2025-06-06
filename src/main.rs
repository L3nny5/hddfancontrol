//! Control fan speed according to drive temperature

#![cfg_attr(
    feature = "gen-man-pages",
    expect(dead_code, unused_crate_dependencies, unused_imports)
)]

use std::{
    ops::Range,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use std::io::Write;
use std::fs;
use std::os::unix::fs::symlink;

use anyhow::Context as _;
use byte_unit::Byte;
use chrono::Local;
use clap::Parser as _;
use device::Hwmon;
use exit::ExitHook;
use fan::Speed;
use flexi_logger::{Logger, FileSpec, Criterion, Naming, Cleanup, Duplicate};
use once_cell::sync::OnceCell;
use probe::Temp;

static FORMAT_STRING: OnceCell<String> = OnceCell::new();

mod cl;
mod device;
mod exit;
mod fan;
mod probe;
mod pwm;
mod sysfs;
#[cfg(test)]
mod tests;

use crate::{device::Drive, fan::Fan, probe::DeviceTempProber};

/// Interruptible sleep
fn sleep(dur: Duration, exit_rx: &mpsc::Receiver<()>) {
    let _ = exit_rx.recv_timeout(dur);
}

#[cfg(feature = "gen-man-pages")]
fn main() -> anyhow::Result<()> {
    use clap::CommandFactory as _;
    let cmd = cl::Args::command();
    let output = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Missing output dir argument"))?;
    clap_mangen::generate_to(cmd, output)?;
    Ok(())
}

#[cfg(not(feature = "gen-man-pages"))]
#[expect(clippy::too_many_lines)]
fn main() -> anyhow::Result<()> {
    // Parse cl args
    let args = cl::Args::parse();

   // Validate the provided datetime format
    if let Err(e) = std::panic::catch_unwind(|| {
        let _ = Local::now().format(&args.log_datetime_format).to_string();
    }) {
        anyhow::bail!("Invalid datetime format string '{}': {e:?}", args.log_datetime_format);
    }

    match args.command {
        cl::Command::PwmTest { pwm } => {
            for pwm_path in &pwm {
                let fan = Fan::new(&cl::PwmSettings {
                    filepath: pwm_path.to_owned(),
                    thresholds: fan::Thresholds::default(),
                })
                .context("Failed to setup fan")?;
                let rpm_path = fan
                    .resolve_rpm_path()
                    .context("Failed to resolve fan rpm filepath")?;
                let mut fan = fan
                    .with_rpm_file(&rpm_path)
                    .context("Failed to setup fan with rpm filepath")?;
                log::info!("Testing fan {fan}, this may take a long time");
                match fan.test() {
                    Ok(t) => {
                        log::info!("Fan {fan}] start/stop thresholds: {t}");
                    }
                    Err(e) => {
                        log::error!("Fan {fan} test failed: {e}");
                    }
                }
            }
        }
        cl::Command::Daemon {
            drives: drive_selectors,
            hddtemp_daemon_port,
            pwm,
            drive_temp_range,
            min_fan_speed_prct,
            interval,
            hwmons,
            restore_fan_settings,
            log_dir,
            log_max_size,
            log_retain,
            log_datetime_format,
        } => {
            // Configure logging
            let log_max_size_bytes = Byte::parse_str(&log_max_size, true)
                .with_context(|| format!("Invalid value for --log-max-size: {}", log_max_size))?
                .as_u64();

            // Set global timestamp format
            FORMAT_STRING.set(log_datetime_format.clone()).unwrap();

            // Custom log formatter
            fn my_format(
                writer: &mut dyn Write,
                now: &mut flexi_logger::DeferredNow,
                record: &log::Record,
            ) -> std::io::Result<()> {
                write!(
                    writer,
                    "[{}] [{}] {}",
                    now.format(&FORMAT_STRING.get().unwrap()),
                    record.level(),
                    record.args()
                )
            }

            // Ensure `logs/` subdirectory exists
            let logs_dir = log_dir.join("logs");
            fs::create_dir_all(&logs_dir)
                .with_context(|| format!("Failed to create logs directory {}", logs_dir.display()))?;

            // Prepare FileSpec for rotated files in `logs/`
            let file_spec = FileSpec::default()
                .directory(&logs_dir)
                .basename("log")      // produces files like log_rCURRENT.log, log_r00001.log, ...
                .suffix("log")
                .suppress_timestamp();

            // Build and start the logger:
            //    - write into `logs/` with rotation
            //    - duplicate all levels to stdout
            //    - create a stable symlink in the parent dir
            let logger = Logger::try_with_str(args.verbosity.to_string())?
                .format(my_format)
                .log_to_file(file_spec)
                .rotate(
                    Criterion::Size(log_max_size_bytes),
                    Naming::Numbers,
                    Cleanup::KeepLogFiles(log_retain),
                )
                .duplicate_to_stdout(Duplicate::All);

            // start the logger
            logger.start()?;

            // use the local `log_dir` you destructured above
            let link_path   = log_dir.join("hddfancontrol.log");
            // target_path: /var/log/hddfancontrol/logs/log_rCURRENT.log
            let target_path = logs_dir.join("log_rCURRENT.log");

            // remove existing link or stale file if present
            if link_path.exists() {
                fs::remove_file(&link_path)
                    .with_context(|| format!("Failed to remove old symlink {}", link_path.display()))?;
            }

            // create a new symlink `hddfancontrol.log` → `logs/log_rCURRENT.log`
            symlink(&target_path, &link_path)
                .with_context(|| {
                    format!(
                        "Failed to create symlink {} -> {}",
                        link_path.display(),
                        target_path.display()
                    )
                })?;

            // DEBUG: confirm symlink creation
            println!("Symlink created: {} -> {}", link_path.display(), target_path.display());

            #[expect(clippy::indexing_slicing)] // guaranteed by clap's numl_args
            let drive_temp_range = Range {
                start: drive_temp_range[0],
                end: drive_temp_range[1],
            };
            let drive_paths: Vec<PathBuf> = drive_selectors
                .into_iter()
                .map(|s| {
                    s.to_drive_paths()
                        .with_context(|| format!("Failed to match drives for selector {s}"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();
            anyhow::ensure!(!drive_paths.is_empty(), "No drive match");
            let drives: Vec<Drive> = drive_paths
                .iter()
                .map(|path| Drive::new(path))
                .collect::<anyhow::Result<_>>()
                .context("Failed to setup drives")?;
            let mut drive_probers: Vec<(Box<dyn DeviceTempProber>, bool)> = drives
                .iter()
                .zip(drive_paths.iter())
                .map(|(drive, path)| {
                    probe::prober(drive, hddtemp_daemon_port)
                        .with_context(|| format!("Failed to setup prober for drive {drive}"))?
                        .ok_or_else(|| {
                            anyhow::anyhow!("No probing method found for drive {path:?}")
                        })
                })
                .collect::<anyhow::Result<_>>()
                .context("Failed to setup drive probers")?;

            let mut hwmon_and_range: Vec<(Hwmon, Range<Temp>)> = hwmons
                .iter()
                .map(|h| {
                    let hwm = Hwmon::new(&h.filepath)
                        .with_context(|| format!("Failed to setup hwmon {:?}", h.filepath))?;
                    let range = h.temp.as_ref().map_or_else(
                        || -> anyhow::Result<_> {
                            // Default range
                            let range = hwm.default_range().with_context(|| {
                                format!(
                                    "Failed to compute default temperature range for hwmon {hwm}"
                                )
                            })?;
                            log::info!(
                                "Device temperature range set to {}-{}°C",
                                range.start,
                                range.end
                            );
                            Ok(range)
                        },
                        |r| Ok(r.clone()),
                    )?;
                    Ok((hwm, range))
                })
                .collect::<anyhow::Result<_>>()?;

            let min_fan_speed = Speed::try_from(f64::from(min_fan_speed_prct) / 100.0)
                .with_context(|| format!("Invalid speed {min_fan_speed_prct}%"))?;
            let mut fans: Vec<_> = pwm
                .iter()
                .map(Fan::new)
                .collect::<anyhow::Result<_>>()
                .context("Failed to setup fans")?;

            let _exit_hook = ExitHook::new(
                pwm.iter()
                    .map(|p| pwm::Pwm::new(&p.filepath))
                    .collect::<anyhow::Result<_>>()
                    .context("Failed to setup PWMs for exit hook")?,
                restore_fan_settings,
            )?;

            // Signal handling
            let exit_requested = Arc::new(AtomicBool::new(false));
            let (exit_tx, exit_rx) = mpsc::channel::<()>();
            {
                let exit_requested = Arc::clone(&exit_requested);
                ctrlc::set_handler(move || {
                    exit_requested.store(true, Ordering::SeqCst);
                    let _ = exit_tx.send(());
                })
                .context("Failed to setup SIGINT handler")?;
            }

            while !exit_requested.load(Ordering::SeqCst) {
                let start = Instant::now();

                let max_drive_temp = drive_probers
                    .iter_mut()
                    .zip(drives.iter())
                    .map(|((prober, supports_probing_sleeping), drive)| {
                        let state = drive
                            .state()
                            .with_context(|| format!("Failed to get drive {drive} state"))?;
                        log::debug!("Drive {drive} state: {state}");
                        let temp = if state.is_spun_down() && !*supports_probing_sleeping {
                            log::debug!("Drive {drive} is sleeping");
                            None
                        } else {
                            let temp = prober
                                .probe_temp()
                                .with_context(|| format!("Failed to get drive {drive} temp"))?;
                            log::debug!("Drive {drive}: {temp}°C");
                            Some(temp)
                        };
                        Ok(temp)
                    })
                    .collect::<anyhow::Result<Vec<_>>>()
                    .context("Failed to get maximum drive temperature")?
                    .into_iter()
                    .flatten()
                    .reduce(f64::max);

                let hwmon_temps: Vec<Temp> = hwmon_and_range
                    .iter_mut()
                    .map(|(hwm, _range)| {
                        let temp = hwm
                            .probe_temp()
                            .with_context(|| format!("Failed to get hwmon {hwm} temp"))?;
                        log::info!("Hwmon {hwm} temperature: {temp}°C");
                        Ok(temp)
                    })
                    .collect::<anyhow::Result<_>>()?;

                let mut speed = min_fan_speed;
                if let Some(max_drive_temp) = max_drive_temp {
                    log::info!("Max drive temperature: {max_drive_temp}°C");
                    speed = fan::target_speed(max_drive_temp, &drive_temp_range, speed);
                } else {
                    log::info!("All drives are spun down");
                }
                for (hwmon_temp, (_hwmon, hwmon_range)) in
                    hwmon_temps.into_iter().zip(hwmon_and_range.iter())
                {
                    speed = fan::target_speed(hwmon_temp, hwmon_range, speed);
                }
                for fan in &mut fans {
                    fan.set_speed(speed)
                        .with_context(|| format!("Failed to set fan {fan} speed"))?;
                }

                let elapsed = Instant::now().duration_since(start);
                let to_wait = interval.saturating_sub(elapsed);
                log::debug!("Will sleep at most {to_wait:?}");
                sleep(to_wait, &exit_rx);
            }
        }
    }

    Ok(())
}
