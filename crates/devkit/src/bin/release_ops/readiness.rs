//! Read-after-success consistency only; mutations are never retried here.
use super::*;
use std::time::{Duration, Instant};

pub(super) const BUDGET: Duration = Duration::from_secs(360);
const INTERVAL: Duration = Duration::from_secs(10);

pub(super) fn wait(
    name: &str,
    version: &str,
    elapsed: impl FnMut() -> Duration,
    read: impl FnMut(Duration) -> Result<Option<PublishedNpm>>,
    pause: impl FnMut(Duration) -> Result<()>,
) -> Result<PublishedNpm> {
    wait_for(&format!("Published {name}@{version}"), elapsed, read, pause)
}

pub(super) fn wait_latest(
    contract: &Contract,
    acknowledged: &BTreeMap<String, String>,
    pending: Option<&str>,
    elapsed: impl Fn(&str) -> Duration,
    mut read: impl FnMut(Duration) -> Result<Selectors>,
    pause: impl FnMut(Duration) -> Result<()>,
) -> Result<Selectors> {
    ensure!(
        !acknowledged.is_empty(),
        "Latest visibility wait requires a successful npm write"
    );
    for (name, predecessor) in acknowledged {
        ensure!(
            npm_pointer(&contract.previous, name)? == Some(predecessor.as_str())
                && stable(predecessor)? < stable(&contract.version)?,
            "Latest visibility wait requires the exact stable pre-write predecessor"
        );
    }
    ensure!(
        pending.is_none_or(|name| acknowledged.contains_key(name)),
        "Pending latest write was not acknowledged"
    );
    let waiting = std::cell::RefCell::new(
        pending
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>(),
    );
    wait_for(
        &format!("Npm latest promotion to {}", contract.version),
        || {
            let waiting = waiting.borrow();
            if waiting.is_empty() {
                // Previously verified channels permit one fresh bounded
                // snapshot even after their propagation deadlines. A stale
                // value below must still satisfy its original deadline.
                Duration::ZERO
            } else {
                waiting.iter().map(|name| elapsed(name)).max().unwrap()
            }
        },
        |remaining| {
            let snapshot = read(remaining)?;
            // A read of another channel must not hide a conflicting/newer
            // selector while an acknowledged npm write is still propagating.
            guard_selectors(&snapshot, contract)?;
            let mut waiting = waiting.borrow_mut();
            for (name, predecessor) in acknowledged {
                ensure!(
                    !waiting.contains(name) || elapsed(name) < BUDGET,
                    "Npm latest {name} was not readable within {}s; retry against the original frozen set",
                    BUDGET.as_secs()
                );
                let pointer = npm_pointer(&snapshot, name)?;
                if pointer != Some(contract.version.as_str()) {
                    ensure!(
                        pointer == Some(predecessor.as_str()),
                        "Unexpected {name} latest pointer after successful promotion"
                    );
                    ensure!(
                        elapsed(name) < BUDGET,
                        "Npm latest {name} was not readable within {}s; retry against the original frozen set",
                        BUDGET.as_secs()
                    );
                    waiting.insert(name.clone());
                } else {
                    waiting.remove(name);
                }
            }
            Ok(waiting.is_empty().then_some(snapshot))
        },
        pause,
    )
}

fn npm_pointer<'a>(selectors: &'a Selectors, name: &str) -> Result<Option<&'a str>> {
    match name {
        ENVIRONMENT => Ok(selectors.environment.as_deref()),
        CLI => Ok(selectors.cli.as_deref()),
        _ => bail!("Unexpected npm latest package: {name}"),
    }
}

fn wait_for<T>(
    subject: &str,
    mut elapsed: impl FnMut() -> Duration,
    mut read: impl FnMut(Duration) -> Result<Option<T>>,
    mut pause: impl FnMut(Duration) -> Result<()>,
) -> Result<T> {
    let timeout = || {
        format!(
            "{subject} was not readable within {}s; retry against the original frozen set",
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
            eprintln!("{subject} is readable");
            return Ok(value);
        }
        if !announced {
            eprintln!(
                "Waiting for {subject} ({}s total budget); checking reads only",
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

#[cfg(test)]
mod latest_tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn contract() -> Contract {
        Contract {
            schema_version: 1,
            repository: REPO.into(),
            version: "0.3.1".into(),
            source_commit: "a".repeat(40),
            source_tree: "b".repeat(40),
            build_run: "https://github.com/docouno/devkit/actions/runs/123/attempts/1".into(),
            index_sha256: "c".repeat(64),
            previous: Selectors {
                environment: Some("0.3.0".into()),
                cli: Some("0.3.0".into()),
                github: Some("v0.3.0".into()),
            },
        }
    }
    fn acknowledged() -> BTreeMap<String, String> {
        BTreeMap::from([(ENVIRONMENT.into(), "0.3.0".into())])
    }
    fn visible(c: &Contract) -> Selectors {
        Selectors {
            environment: Some(c.version.clone()),
            ..c.previous.clone()
        }
    }

    #[test]
    fn exact_stale_predecessor_waits_and_each_read_uses_the_remaining_budget() {
        let c = contract();
        // Continue an existing promotion budget rather than granting another
        // 360 seconds when the next channel or final snapshot is read.
        let elapsed = Cell::new(Duration::from_secs(17));
        let reads = RefCell::new(Vec::new());
        let pauses = RefCell::new(Vec::new());
        let result = wait_latest(
            &c,
            &acknowledged(),
            Some(ENVIRONMENT),
            |_| elapsed.get(),
            |remaining| {
                reads.borrow_mut().push(remaining.as_secs());
                elapsed.set(elapsed.get() + Duration::from_secs(3));
                Ok(if reads.borrow().len() < 3 {
                    c.previous.clone()
                } else {
                    visible(&c)
                })
            },
            |duration| {
                pauses.borrow_mut().push(duration.as_secs());
                elapsed.set(elapsed.get() + duration);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(result, visible(&c));
        assert_eq!(*reads.borrow(), [343, 330, 317]);
        assert_eq!(*pauses.borrow(), [10, 10]);
        assert_eq!(elapsed.get(), Duration::from_secs(46));
    }

    #[test]
    fn deadline_includes_reads_and_last_pause_without_an_extra_request() {
        let c = contract();
        let elapsed = Cell::new(Duration::ZERO);
        let reads = Cell::new(0);
        let error = wait_latest(
            &c,
            &acknowledged(),
            Some(ENVIRONMENT),
            |_| elapsed.get(),
            |remaining| {
                assert_eq!(remaining, BUDGET);
                reads.set(reads.get() + 1);
                elapsed.set(Duration::from_secs(355));
                Ok(c.previous.clone())
            },
            |duration| {
                assert_eq!(duration, Duration::from_secs(5));
                elapsed.set(elapsed.get() + duration);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("within 360s"));
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn expected_selector_returned_at_or_after_the_deadline_is_not_success() {
        let c = contract();
        for late in [BUDGET, BUDGET + Duration::from_nanos(1)] {
            let elapsed = Cell::new(Duration::from_secs(355));
            let error = wait_latest(
                &c,
                &acknowledged(),
                Some(ENVIRONMENT),
                |_| elapsed.get(),
                |remaining| {
                    assert_eq!(remaining, Duration::from_secs(5));
                    elapsed.set(late);
                    Ok(visible(&c))
                },
                |_| panic!("A late success must not pause"),
            )
            .unwrap_err();
            assert!(error.to_string().contains("original frozen set"));
        }
    }

    #[test]
    fn errors_and_cancelled_pause_stop_without_another_read() {
        let c = contract();
        for kind in ["authorization", "transport", "malformed JSON"] {
            let reads = Cell::new(0);
            let error = wait_latest(
                &c,
                &acknowledged(),
                Some(ENVIRONMENT),
                |_| Duration::ZERO,
                |_| {
                    reads.set(reads.get() + 1);
                    bail!("{kind}")
                },
                |_| panic!("Provider errors must not pause"),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), kind);
            assert_eq!(reads.get(), 1);
        }
        let reads = Cell::new(0);
        let error = wait_latest(
            &c,
            &acknowledged(),
            Some(ENVIRONMENT),
            |_| Duration::ZERO,
            |_| {
                reads.set(reads.get() + 1);
                Ok(c.previous.clone())
            },
            |_| Err(devkit::processes::failure(143, "cancelled")),
        )
        .unwrap_err();
        assert_eq!(devkit::processes::exit_code(&error), 143);
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn absence_malformed_and_conflicting_selectors_in_any_channel_stop() {
        let c = contract();
        let mut invalid = Vec::new();
        for pointer in [None, Some("wrong"), Some("0.3.2"), Some("0.2.9")] {
            let mut snapshot = c.previous.clone();
            snapshot.environment = pointer.map(str::to_owned);
            invalid.push(snapshot);
        }
        for expected in [false, true] {
            let mut snapshot = if expected {
                visible(&c)
            } else {
                c.previous.clone()
            };
            snapshot.cli = Some("0.4.0".into());
            invalid.push(snapshot);
        }
        for pointer in [None, Some("wrong"), Some("v0.3.2")] {
            let mut snapshot = c.previous.clone();
            snapshot.github = pointer.map(str::to_owned);
            invalid.push(snapshot);
        }
        for snapshot in invalid {
            let reads = Cell::new(0);
            assert!(
                wait_latest(
                    &c,
                    &acknowledged(),
                    Some(ENVIRONMENT),
                    |_| Duration::ZERO,
                    |_| {
                        reads.set(reads.get() + 1);
                        Ok(snapshot.clone())
                    },
                    |_| panic!("Unexpected selectors must not pause"),
                )
                .is_err()
            );
            assert_eq!(reads.get(), 1);
        }
    }

    #[test]
    fn no_unacknowledged_or_nonpredecessor_write_can_enter_the_wait() {
        let c = contract();
        for acknowledged in [
            BTreeMap::new(),
            BTreeMap::from([("another package".into(), "0.3.0".into())]),
            BTreeMap::from([(ENVIRONMENT.into(), "0.2.9".into())]),
            BTreeMap::from([(ENVIRONMENT.into(), "0.3.1".into())]),
        ] {
            assert!(
                wait_latest(
                    &c,
                    &acknowledged,
                    Some(ENVIRONMENT),
                    |_| Duration::ZERO,
                    |_| panic!("Invalid acknowledgement must not read"),
                    |_| panic!("Invalid acknowledgement must not pause"),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn a_new_channel_gets_its_own_budget_while_verified_old_channels_can_remain_visible() {
        let c = contract();
        let mut acknowledged = acknowledged();
        acknowledged.insert(CLI.into(), "0.3.0".into());
        let elapsed = Cell::new(Duration::ZERO);
        let reads = RefCell::new(Vec::new());
        let result = wait_latest(
            &c,
            &acknowledged,
            Some(CLI),
            |name| {
                elapsed.get()
                    + if name == ENVIRONMENT {
                        Duration::from_secs(350)
                    } else {
                        Duration::ZERO
                    }
            },
            |remaining| {
                reads.borrow_mut().push(remaining.as_secs());
                elapsed.set(elapsed.get() + Duration::from_secs(5));
                let mut snapshot = visible(&c);
                if reads.borrow().len() == 2 {
                    snapshot.cli = Some(c.version.clone());
                }
                Ok(snapshot)
            },
            |duration| {
                elapsed.set(elapsed.get() + duration);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(result.cli.as_deref(), Some(c.version.as_str()));
        assert_eq!(*reads.borrow(), [360, 345]);
        assert_eq!(elapsed.get(), Duration::from_secs(20));
    }

    #[test]
    fn final_snapshot_accepts_verified_expected_values_after_expiry_but_never_renews_stale_tolerance()
     {
        let c = contract();
        let mut acknowledged = acknowledged();
        acknowledged.insert(CLI.into(), "0.3.0".into());
        for stale in [false, true] {
            let reads = Cell::new(0);
            let elapsed = Cell::new(Duration::from_secs(370));
            let result = wait_latest(
                &c,
                &acknowledged,
                None,
                |_| elapsed.get(),
                |remaining| {
                    assert_eq!(remaining, BUDGET);
                    reads.set(reads.get() + 1);
                    elapsed.set(elapsed.get() + Duration::from_secs(3));
                    let mut snapshot = visible(&c);
                    snapshot.cli = Some(c.version.clone());
                    if stale {
                        snapshot.environment = c.previous.environment.clone();
                    }
                    Ok(snapshot)
                },
                |_| panic!("Expired stale tolerance must not pause"),
            );
            assert_eq!(reads.get(), 1);
            if stale {
                assert!(result.unwrap_err().to_string().contains("within 360s"));
            } else {
                assert_eq!(
                    result.unwrap().environment.as_deref(),
                    Some(c.version.as_str())
                );
            }
        }
    }
}
