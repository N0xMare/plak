use plakous_hosted::sessions::projector::{SessionViews, ViewWrites};
use plakous_hosted::sessions::{NoticeScope, SessionNode, SessionRecord};

use super::*;

#[test]
fn range_matches_the_keyed_store_and_counts_unescaped_body_bytes() {
    let home = PlakPaths::isolated_tempdir();
    let paths = PlakPaths::explicit(home.path().join("home"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut record = SessionRecord::new(home.path().to_owned());
    store.commit(&mut record).unwrap();
    let insert = |key: RowKey, row: &TranscriptRow| {
        let value = serde_json::to_string(row).unwrap();
        store.connection.execute(
                "INSERT INTO operator_notices(session_id,anchor,ordinal,row,notice_id) VALUES(?1,?2,?3,?4,?3)",
                (&record.id, i64::try_from(key.anchor).unwrap(), key.ordinal, value),
        ).unwrap();
    };
    let imported = RowKey::new(0, Lane::Product, 9);
    let answer = RowKey::new(20, Lane::Runtime, 500);
    let notice = RowKey::new(20, Lane::Product, 17);
    insert(imported, &TranscriptRow::user("é\0x"));
    insert(notice, &TranscriptRow::info(NoticeScope::Turn, "résumé"));
    let mut views = SessionViews::empty();
    views.cursor = FeedSeq::new(100);
    views.rows.extend([
        (answer, TranscriptRow::assistant("☕\n\"")),
        (
            RowKey::new(100, Lane::Runtime, 4),
            TranscriptRow::Interrupted,
        ),
    ]);
    store
        .commit_projection_views(
            &record.id,
            &ViewWrites::between(&SessionViews::empty(), &views),
            &views.state,
        )
        .unwrap();

    read(&paths, &record.id, |history| {
        assert_eq!(history.totals(0, Some(20))?, Totals { rows: 1, bytes: 4 });
        assert_eq!(
            history.totals(20, Some(100))?,
            Totals { rows: 2, bytes: 13 }
        );
        let items = history.items(20, Some(100))?;
        assert_eq!(
            items.iter().map(|item| item.key).collect::<Vec<_>>(),
            [answer, notice]
        );
        assert_eq!(history.row(imported)?, TranscriptRow::user("é\0x"));
        assert_eq!(
            history.row(RowKey::new(20, Lane::Runtime, 17)),
            Err(HistoryError::UnknownRow)
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn an_archived_product_child_reads_only_its_own_rows() {
    let home = PlakPaths::isolated_tempdir();
    let paths = PlakPaths::explicit(home.path().join("home"));
    let mut store = ProductStore::open(&paths).unwrap();
    let mut root = SessionRecord::new(home.path().to_owned());
    let mut child = SessionRecord::new(home.path().to_owned());
    child.node = SessionNode::parse(&root.node.tree_id, "child-1").unwrap();
    child.archived = true;
    for (record, body) in [(&mut root, "root body"), (&mut child, "child body")] {
        record.record_notice(TranscriptRow::info(NoticeScope::Session, body));
        store.commit(record).unwrap();
    }
    read(&paths, &child.id, |history| {
        let key = RowKey::new(0, Lane::Product, 0);
        assert_eq!(history.items(0, None)?.len(), 1);
        assert_eq!(history.row(key)?.text(), Some("child body"));
        assert_ne!(history.line.session(), root.id);
        Ok(())
    })
    .unwrap();
}
