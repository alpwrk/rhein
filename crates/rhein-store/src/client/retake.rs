//! The retake ladder: the waits a lost seat's rejoin is retried along, and how a failed join
//! picks its ladder (`web/src/hooks/useVoice.jsx`). An SFU that is restarting is outlived
//! instead of ejecting everyone from the channel; a server that answered no is not argued with.

use std::time::Duration;

use rhein_net::ApiError;

/// Between retake attempts the join is re-run after these waits, so a server that answered no
/// (permissions, a full channel) is retried a few times — the roster may change under it —
/// without outlasting the answer. One attempt per entry; the last one failing gives the seat up.
pub static RETAKE_BACKOFF: [Duration; 5] =
    [Duration::from_secs(3), Duration::from_secs(6), Duration::from_secs(10), Duration::from_secs(15), Duration::from_secs(20)];

/// When the join cannot even reach a server — a homeserver or SFU restarting for a couple of
/// minutes — the seat is bridged along this longer ladder rather than given up, because the
/// answer will change when it comes back.
pub static RETAKE_UNREACHABLE_BACKOFF: [Duration; 11] = [
    Duration::from_secs(3),
    Duration::from_secs(6),
    Duration::from_secs(10),
    Duration::from_secs(15),
    Duration::from_secs(20),
    Duration::from_secs(30),
    Duration::from_secs(45),
    Duration::from_secs(60),
    Duration::from_secs(90),
    Duration::from_secs(90),
    Duration::from_secs(90),
];

/// True when a join failed for a reason that time can fix: nobody answered, or the answer was
/// 'busy'. A server that answered 'no' gets the short ladder instead — retrying cannot change
/// that answer.
pub fn join_unanswered(err: &ApiError) -> bool {
    unanswered(err.status())
}

/// The same, read off the status alone; no status means the request never got an answer (the
/// network is down).
fn unanswered(status: Option<u16>) -> bool {
    match status {
        Some(status) => status == 408 || status == 429 || status >= 500,
        None => true,
    }
}

/// The ladder a failed join is retried along.
pub fn ladder(err: &ApiError) -> &'static [Duration] {
    if join_unanswered(err) { &RETAKE_UNREACHABLE_BACKOFF } else { &RETAKE_BACKOFF }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(status: u16) -> ApiError {
        let body = rhein_proto::ErrorBody { error: "no".into(), code: None, message: None, retry_after: None };
        ApiError::Status { status, body }
    }

    #[test]
    fn a_server_that_never_answered_or_said_it_is_busy_is_bridged() {
        assert!(unanswered(None), "no answer at all");
        for status in [408, 429, 500, 502, 503] {
            assert!(unanswered(Some(status)), "status {status}");
        }
    }

    #[test]
    fn a_server_that_answered_no_is_not_argued_with() {
        for status in [400, 401, 403, 404, 409] {
            assert!(!unanswered(Some(status)), "status {status}");
        }
    }

    #[test]
    fn the_unreachable_ladder_strictly_outlives_the_ordinary_one() {
        let total = |ladder: &[Duration]| ladder.iter().map(|d| d.as_secs()).sum::<u64>();
        assert!(RETAKE_UNREACHABLE_BACKOFF.len() > RETAKE_BACKOFF.len());
        assert!(total(&RETAKE_UNREACHABLE_BACKOFF) > 300);
        assert!(total(&RETAKE_BACKOFF) < 120);
        assert_eq!(ladder(&refused(503)), &RETAKE_UNREACHABLE_BACKOFF[..]);
        assert_eq!(ladder(&refused(403)), &RETAKE_BACKOFF[..]);
    }
}
