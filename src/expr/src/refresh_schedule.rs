// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use proptest::arbitrary::{any, Arbitrary};
use proptest::prelude::{BoxedStrategy, Strategy};
use std::time::Duration;

use mz_proto::IntoRustIfSome;
use mz_proto::{ProtoType, RustType, TryFromProtoError};
use mz_repr::Timestamp;
use serde::{Deserialize, Serialize};

include!(concat!(env!("OUT_DIR"), "/mz_expr.refresh_schedule.rs"));

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct RefreshSchedule {
    // `REFRESH EVERY`s
    pub everies: Vec<RefreshEvery>,
    // `REFRESH AT`s
    pub ats: Vec<Timestamp>,
}

impl RefreshSchedule {
    pub fn empty() -> RefreshSchedule {
        RefreshSchedule {
            everies: Vec::new(),
            ats: Vec::new(),
        }
    }

    /// Rounds up the timestamp to the time of the next refresh.
    /// Returns None if there is no next refresh.
    /// Note that this fn is monotonic.
    pub fn round_up_timestamp(&self, timestamp: Timestamp) -> Option<Timestamp> {
        let next_every = self
            .everies
            .iter()
            .map(|refresh_every| refresh_every.round_up_timestamp(timestamp))
            .min();
        let next_at = self
            .ats
            .iter()
            .filter(|at| **at >= timestamp)
            .min()
            .cloned();
        // Take the min of `next_every` and `next_at`, but with considering None to be bigger than
        // any Some. Note: Simply `min(next_every, next_at)` wouldn't do what we want, because None
        // is smaller than any Some.
        next_every.into_iter().chain(next_at).min()
    }

    /// Returns the time of the last refresh. Returns None if there is no last refresh (e.g., for a
    /// periodic refresh).
    pub fn last_refresh(&self) -> Option<Timestamp> {
        if self.everies.is_empty() {
            self.ats.iter().max().cloned()
        } else {
            None
        }
    }

    /// Returns whether the schedule is empty, i.e., no `EVERY` or `AT`.
    pub fn is_empty(&self) -> bool {
        self.everies.is_empty() && self.ats.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct RefreshEvery {
    pub interval: Duration,
    pub aligned_to: Timestamp,
}

impl RefreshEvery {
    /// Rounds up the timestamp to the time of the next refresh, according to the given periodic refresh schedule.
    /// It saturates, i.e., if the rounding would make it overflow, then it returns the maximum possible timestamp.
    ///
    /// # Panics
    /// - if the refresh interval converted to milliseconds cast to u64 overflows;
    /// - if the interval is 0.
    /// (These should be checked in HIR planning.)
    pub fn round_up_timestamp(&self, timestamp: Timestamp) -> Timestamp {
        let RefreshEvery {
            interval,
            aligned_to,
        } = self;
        let interval = u64::try_from(interval.as_millis()).unwrap();
        // Rounds up `x` to the nearest multiple of `interval`.
        let round_up_to_multiple_of_interval = |x: u64| -> u64 {
            assert_ne!(x, 0);
            (((x - 1) / interval) + 1).saturating_mul(interval)
        };
        // Rounds down `x` to the nearest multiple of `interval`.
        let round_down_to_multiple_of_interval = |x: u64| -> u64 { x / interval * interval };
        let result =
            if timestamp > *aligned_to {
                Timestamp::new(u64::from(aligned_to).saturating_add(
                    round_up_to_multiple_of_interval(u64::from(timestamp) - u64::from(aligned_to)),
                ))
            } else {
                // Note: `timestamp == aligned_to` has to be handled here, because in the other branch
                // `x - 1` would underflow.
                //
                // Also, no need to check for overflows here, since all the numbers are either between
                // `timestamp` and `aligned_to`, or not greater than `aligned_to.internal - self.internal`.
                Timestamp::new(
                    u64::from(aligned_to)
                        - round_down_to_multiple_of_interval(
                            u64::from(aligned_to) - u64::from(timestamp),
                        ),
                )
            };
        // TODO: Downgrade these to non-logging soft asserts when we have built more confidence in the code.
        assert!(u64::from(result) >= u64::from(timestamp));
        assert!(u64::from(result) - u64::from(timestamp) <= interval);
        result
    }
}

impl RustType<ProtoRefreshSchedule> for RefreshSchedule {
    fn into_proto(&self) -> ProtoRefreshSchedule {
        ProtoRefreshSchedule {
            everies: self.everies.into_proto(),
            ats: self.ats.into_proto(),
        }
    }

    fn from_proto(proto: ProtoRefreshSchedule) -> Result<Self, TryFromProtoError> {
        Ok(RefreshSchedule {
            everies: proto.everies.into_rust()?,
            ats: proto.ats.into_rust()?,
        })
    }
}

impl RustType<ProtoRefreshEvery> for RefreshEvery {
    fn into_proto(&self) -> ProtoRefreshEvery {
        ProtoRefreshEvery {
            interval: Some(self.interval.into_proto()),
            aligned_to: Some(self.aligned_to.into_proto()),
        }
    }

    fn from_proto(proto: ProtoRefreshEvery) -> Result<Self, TryFromProtoError> {
        Ok(RefreshEvery {
            interval: proto
                .interval
                .into_rust_if_some("ProtoRefreshEvery::interval")?,
            aligned_to: proto
                .aligned_to
                .into_rust_if_some("ProtoRefreshEvery::aligned_to")?,
        })
    }
}

impl Arbitrary for RefreshSchedule {
    type Strategy = BoxedStrategy<Self>;
    type Parameters = ();

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        (
            proptest::collection::vec(any::<RefreshEvery>(), 0..4),
            proptest::collection::vec(any::<Timestamp>(), 0..4),
        )
            .prop_map(|(everies, ats)| RefreshSchedule { everies, ats })
            .boxed()
    }
}

impl Arbitrary for RefreshEvery {
    type Strategy = BoxedStrategy<Self>;
    type Parameters = ();

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        (any::<Duration>(), any::<Timestamp>())
            .prop_map(|(interval, aligned_to)| RefreshEvery {
                interval,
                aligned_to,
            })
            .boxed()
    }
}

#[cfg(test)]
mod tests {
    use crate::refresh_schedule::{RefreshEvery, RefreshSchedule};
    use mz_repr::adt::interval::Interval;
    use mz_repr::Timestamp;
    use std::str::FromStr;

    #[mz_ore::test]
    fn test_round_up_timestamp() {
        let ts = |t: u64| Timestamp::new(t);
        let test = |schedule: RefreshSchedule| {
            move |expected_ts: Option<u64>, input_ts| {
                assert_eq!(
                    expected_ts.map(ts),
                    schedule.round_up_timestamp(ts(input_ts))
                )
            }
        };
        {
            let schedule = RefreshSchedule {
                everies: vec![],
                ats: vec![ts(123), ts(456)],
            };
            let test = test(schedule);
            test(Some(123), 0);
            test(Some(123), 50);
            test(Some(123), 122);
            test(Some(123), 123);
            test(Some(456), 124);
            test(Some(456), 130);
            test(Some(456), 455);
            test(Some(456), 456);
            test(None, 457);
            test(None, 12345678);
            test(None, u64::MAX - 1000);
            test(None, u64::MAX - 1);
            test(None, u64::MAX);
        }
        {
            let schedule = RefreshSchedule {
                everies: vec![RefreshEvery {
                    interval: Interval::from_str("100 milliseconds")
                        .unwrap()
                        .duration()
                        .unwrap(),
                    aligned_to: ts(500),
                }],
                ats: vec![],
            };
            let test = test(schedule);
            test(Some(0), 0);
            test(Some(100), 1);
            test(Some(100), 2);
            test(Some(100), 99);
            test(Some(100), 100);
            test(Some(200), 101);
            test(Some(200), 102);
            test(Some(200), 199);
            test(Some(200), 200);
            test(Some(300), 201);
            test(Some(400), 400);
            test(Some(500), 401);
            test(Some(500), 450);
            test(Some(500), 499);
            test(Some(500), 500);
            test(Some(600), 501);
            test(Some(600), 599);
            test(Some(600), 600);
            test(Some(700), 601);
            test(Some(5434532600), 5434532599);
            test(Some(5434532600), 5434532600);
            test(Some(5434532700), 5434532601);
            test(Some(u64::MAX), u64::MAX - 1);
            test(Some(u64::MAX), u64::MAX);
        }
        {
            let schedule = RefreshSchedule {
                everies: vec![RefreshEvery {
                    interval: Interval::from_str("100 milliseconds")
                        .unwrap()
                        .duration()
                        .unwrap(),
                    aligned_to: ts(542),
                }],
                ats: vec![],
            };
            let test = test(schedule);
            test(Some(42), 0);
            test(Some(42), 1);
            test(Some(42), 41);
            test(Some(42), 42);
            test(Some(142), 43);
            test(Some(442), 441);
            test(Some(442), 442);
            test(Some(542), 443);
            test(Some(542), 541);
            test(Some(542), 542);
            test(Some(642), 543);
            test(Some(u64::MAX), u64::MAX - 1);
            test(Some(u64::MAX), u64::MAX);
        }
        {
            let schedule = RefreshSchedule {
                everies: vec![
                    RefreshEvery {
                        interval: Interval::from_str("100 milliseconds")
                            .unwrap()
                            .duration()
                            .unwrap(),
                        aligned_to: ts(400),
                    },
                    RefreshEvery {
                        interval: Interval::from_str("100 milliseconds")
                            .unwrap()
                            .duration()
                            .unwrap(),
                        aligned_to: ts(542),
                    },
                ],
                ats: vec![ts(2), ts(300), ts(400), ts(471), ts(541), ts(123456)],
            };
            let test = test(schedule);
            test(Some(0), 0);
            test(Some(2), 1);
            test(Some(2), 2);
            test(Some(42), 3);
            test(Some(42), 41);
            test(Some(42), 42);
            test(Some(100), 43);
            test(Some(100), 99);
            test(Some(100), 100);
            test(Some(142), 101);
            test(Some(142), 141);
            test(Some(142), 142);
            test(Some(200), 143);
            test(Some(300), 243);
            test(Some(300), 299);
            test(Some(300), 300);
            test(Some(342), 301);
            test(Some(400), 343);
            test(Some(400), 399);
            test(Some(400), 400);
            test(Some(442), 401);
            test(Some(442), 441);
            test(Some(442), 442);
            test(Some(471), 443);
            test(Some(471), 470);
            test(Some(471), 471);
            test(Some(500), 472);
            test(Some(500), 472);
            test(Some(500), 500);
            test(Some(541), 501);
            test(Some(541), 540);
            test(Some(541), 541);
            test(Some(542), 542);
            test(Some(600), 543);
            test(Some(65500), 65454);
            test(Some(87842), 87831);
            test(Some(123442), 123442);
            test(Some(123456), 123443);
            test(Some(123456), 123456);
            test(Some(123500), 123457);
            test(Some(u64::MAX), u64::MAX - 1);
            test(Some(u64::MAX), u64::MAX);
        }
    }
}
