//! Read the retained fields without applying candidate store admission.

use super::{Duration, Fixture, Instant, PlakPaths, WAIT};
use plakous::sessions::SessionLocator;
use rusqlite::{Connection, OpenFlags, OptionalExtension as _};

pub(super) fn open(fixture: &Fixture) -> Connection {
    let database = PlakPaths::explicit(fixture.home()).product_database();
    let product = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    product.busy_timeout(Duration::from_secs(1)).unwrap();
    product
}

pub(super) fn idle_locator(product: &Connection, session: &str, expected: i64) -> SessionLocator {
    let deadline = Instant::now() + WAIT;
    loop {
        let (locator, turns, open) = product
            .query_row(
                "SELECT s.locator, v.root_turn_count, EXISTS(
                     SELECT 1 FROM session_open_turn_views WHERE session_id=s.id
                 ) FROM sessions s JOIN session_views v ON v.session_id=s.id WHERE s.id=?1",
                [session],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()
            .unwrap()
            .unwrap_or((None, 0, true));
        if turns >= expected && !open {
            assert_eq!(turns, expected, "exactly one root wake");
            return locator.unwrap().parse().unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "root did not finish turn {expected}: turns={turns}, open={open}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
