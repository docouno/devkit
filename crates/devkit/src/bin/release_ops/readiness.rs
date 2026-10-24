//! Read-after-publish consistency only; publication itself is never retried here.
use super::*;
use std::time::{Duration, Instant};

pub(super) const BUDGET: Duration = Duration::from_secs(360);
const INTERVAL: Duration = Duration::from_secs(10);

pub(super) fn wait(
    name: &str,
    version: &str,
    mut elapsed: impl FnMut() -> Duration,
    mut read: impl FnMut(Duration) -> Result<Option<PublishedNpm>>,
    mut pause: impl FnMut(Duration) -> Result<()>,
) -> Result<PublishedNpm> {
    let timeout = || {
        format!(
            "Published {name}@{version} was not readable within {}s; retry against the original frozen set",
            BUDGET.as_secs()
        )
    };
    let mut announced = false;
    loop {
        let remaining = BUDGET
            .checked_sub(elapsed())
            .filter(|value| !value.is_zero())
            .context(timeout())?;
        let result = read(remaining)?;
        // The read/fetch time belongs to the same budget, even when a fake or
        // an unexpectedly late provider returns a successful value.
        ensure!(elapsed() < BUDGET, "{}", timeout());
        if let Some(value) = result {
            eprintln!(
                "Published {name}@{version} is readable after {:.1}s",
                elapsed().as_secs_f64()
            );
            return Ok(value);
        }
        if !announced {
            eprintln!(
                "Waiting for published {name}@{version} ({}s total budget); checking registry reads only",
                BUDGET.as_secs()
            );
            announced = true;
        }
        let remaining = BUDGET.checked_sub(elapsed()).context(timeout())?;
        pause(INTERVAL.min(remaining))?;
    }
}

pub(super) fn pause(runner: &ProcessRunner, duration: Duration) -> Result<()> {
    let deadline = Instant::now() + duration;
    loop {
        runner.check_cancelled()?;
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn visible() -> PublishedNpm {
        PublishedNpm {
            integrity: "test integrity".into(),
            shasum: "test shasum".into(),
            bytes: b"published bytes".to_vec(),
        }
    }

    #[test]
    fn first_visible_read_returns_without_a_pause() {
        let reads = Cell::new(0);
        let result = wait(
            ENVIRONMENT,
            "0.3.1",
            || Duration::ZERO,
            |remaining| {
                reads.set(reads.get() + 1);
                assert_eq!(remaining, BUDGET);
                Ok(Some(visible()))
            },
            |_| panic!("A readable version must not pause"),
        )
        .unwrap();
        assert_eq!(reads.get(), 1);
        assert_eq!(result.bytes, b"published bytes");
    }

    #[test]
    fn absence_waits_charge_both_read_and_pause_time_to_one_budget() {
        let elapsed = Cell::new(Duration::ZERO);
        let reads = RefCell::new(Vec::new());
        let pauses = RefCell::new(Vec::new());
        wait(
            ENVIRONMENT,
            "0.3.1",
            || elapsed.get(),
            |remaining| {
                reads.borrow_mut().push(remaining.as_secs());
                elapsed.set(elapsed.get() + Duration::from_secs(3));
                Ok((reads.borrow().len() == 3).then(visible))
            },
            |duration| {
                pauses.borrow_mut().push(duration.as_secs());
                elapsed.set(elapsed.get() + duration);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*reads.borrow(), [360, 347, 334]);
        assert_eq!(*pauses.borrow(), [10, 10]);
        assert_eq!(elapsed.get(), Duration::from_secs(29));
    }

    #[test]
    fn deadline_includes_the_last_read_and_starts_no_extra_request() {
        let elapsed = Cell::new(Duration::ZERO);
        let reads = Cell::new(0);
        let error = wait(
            ENVIRONMENT,
            "0.3.1",
            || elapsed.get(),
            |remaining| {
                assert_eq!(remaining, BUDGET);
                reads.set(reads.get() + 1);
                elapsed.set(Duration::from_secs(355));
                Ok(None)
            },
            |duration| {
                assert_eq!(duration, Duration::from_secs(5));
                elapsed.set(elapsed.get() + duration);
                Ok(())
            },
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("within 360s"));
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn late_read_success_is_not_accepted_after_the_deadline() {
        let elapsed = Cell::new(Duration::ZERO);
        let error = wait(
            ENVIRONMENT,
            "0.3.1",
            || elapsed.get(),
            |_| {
                elapsed.set(BUDGET + Duration::from_nanos(1));
                Ok(Some(visible()))
            },
            |_| panic!("A late success must stop"),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("original frozen set"));
    }

    #[test]
    fn read_errors_never_become_absence_or_another_request() {
        for kind in ["authorization", "transport", "malformed JSON"] {
            let reads = Cell::new(0);
            let error = wait(
                ENVIRONMENT,
                "0.3.1",
                || Duration::ZERO,
                |_| {
                    reads.set(reads.get() + 1);
                    bail!("{kind}")
                },
                |_| panic!("Provider errors must not pause"),
            )
            .err()
            .unwrap();
            assert_eq!(error.to_string(), kind);
            assert_eq!(reads.get(), 1);
        }
    }

    #[test]
    fn cancelled_pause_does_not_read_again() {
        let reads = Cell::new(0);
        let error = wait(
            ENVIRONMENT,
            "0.3.1",
            || Duration::ZERO,
            |_| {
                reads.set(reads.get() + 1);
                Ok(None)
            },
            |_| Err(devkit::processes::failure(143, "cancelled")),
        )
        .err()
        .unwrap();
        assert_eq!(devkit::processes::exit_code(&error), 143);
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn real_pause_checks_cancellation_before_sleeping() {
        let runner = ProcessRunner::new().unwrap();
        signal_hook::low_level::raise(libc::SIGTERM).unwrap();
        let error = pause(&runner, INTERVAL).unwrap_err();
        assert_eq!(devkit::processes::exit_code(&error), 143);
    }
}
