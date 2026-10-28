//! 開発用の起動計測。通常ビルドでは時計の読取もファイル出力も行わない。
#[cfg(feature = "startup-profile")]
mod enabled {
    use std::{cell::RefCell, io::Write, path::PathBuf, time::Instant};

    struct Profile {
        path: PathBuf,
        start: Instant,
        previous: Instant,
        stages: Vec<(&'static str, f64)>,
    }

    thread_local! {
        static PROFILE: RefCell<Option<Profile>> = const { RefCell::new(None) };
    }

    pub fn start() {
        let now = Instant::now();
        // Python側もclock_gettime(CLOCK_MONOTONIC)を使い、exec直前との差を取る。
        let mut clock = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let clock_ok = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) } == 0;
        let entry_ns = clock.tv_sec as u128 * 1_000_000_000 + clock.tv_nsec as u128;
        if let Some(path) = std::env::var_os("FUMI_PROFILE") {
            let mut stages = Vec::new();
            if clock_ok
                && let Some(exec_ns) = std::env::var("FUMI_PROFILE_EXEC_NS")
                    .ok()
                    .and_then(|s| s.parse::<u128>().ok())
                && let Some(elapsed) = entry_ns.checked_sub(exec_ns)
            {
                stages.push(("exec_to_main_ms", elapsed as f64 / 1_000_000.0));
            }
            PROFILE.with_borrow_mut(|slot| {
                *slot = Some(Profile {
                    path: path.into(),
                    start: now,
                    previous: now,
                    stages,
                });
            });
        }
    }

    pub fn mark(name: &'static str) {
        PROFILE.with_borrow_mut(|slot| {
            if let Some(profile) = slot {
                let now = Instant::now();
                profile.stages.push((
                    name,
                    now.duration_since(profile.previous).as_secs_f64() * 1000.0,
                ));
                profile.previous = now;
            }
        });
    }

    pub fn finish() -> std::io::Result<()> {
        mark("draw_ms");
        PROFILE.with_borrow_mut(|slot| {
            if let Some(mut profile) = slot.take() {
                profile.stages.push((
                    "main_to_flush_ms",
                    profile.previous.duration_since(profile.start).as_secs_f64() * 1000.0,
                ));
                let fields = profile
                    .stages
                    .iter()
                    .map(|(key, value)| format!("\"{key}\":{value:.6}"))
                    .collect::<Vec<_>>()
                    .join(",");
                // 初回flushの計時を終えてから、明示された計測先へ一度だけ出力する。
                // 入力文書など、既存のファイルを計測先として上書きしない。
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(profile.path)?
                    .write_all(format!("{{{fields}}}\n").as_bytes())?;
            }
            Ok(())
        })
    }
}

#[cfg(feature = "startup-profile")]
pub use enabled::{finish, mark, start};

#[cfg(not(feature = "startup-profile"))]
#[inline(always)]
pub fn start() {}
#[cfg(not(feature = "startup-profile"))]
#[inline(always)]
pub fn mark(_: &'static str) {}
#[cfg(not(feature = "startup-profile"))]
#[inline(always)]
pub fn finish() -> std::io::Result<()> {
    Ok(())
}
