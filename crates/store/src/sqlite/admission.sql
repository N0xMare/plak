CREATE INDEX admission_object_codec ON runtime_objects(codec);
CREATE INDEX admission_feed_version ON session_feed(session_id,seq)
 WHERE substr(record,1,13)<>x'7b2276657273696f6e223a312c';

CREATE TABLE admission_counts (
 id INTEGER PRIMARY KEY CHECK(id=1),
 sessions INTEGER NOT NULL CHECK(sessions>=0),
 branches INTEGER NOT NULL CHECK(branches>=0),
 checkpoints INTEGER NOT NULL CHECK(checkpoints>=0),
 objects INTEGER NOT NULL CHECK(objects>=0),
 object_bytes INTEGER NOT NULL CHECK(object_bytes>=0),
 tombstones INTEGER NOT NULL CHECK(tombstones>=0)
) STRICT;

INSERT INTO admission_counts SELECT 1,
 (SELECT count(*) FROM sessions),
 (SELECT count(*) FROM branches),
 (SELECT count(*) FROM checkpoints),
 count(*), COALESCE(sum(byte_len),0)+(SELECT count(*)*37 FROM runtime_object_edges),
 (SELECT count(*) FROM delete_tombstones)
 FROM runtime_objects;

CREATE TRIGGER admission_session_insert AFTER INSERT ON sessions BEGIN
 UPDATE admission_counts SET sessions=sessions+1 WHERE id=1;
END;
CREATE TRIGGER admission_session_delete AFTER DELETE ON sessions BEGIN
 UPDATE admission_counts SET sessions=sessions-1 WHERE id=1;
END;
CREATE TRIGGER admission_branch_insert AFTER INSERT ON branches BEGIN
 UPDATE admission_counts SET branches=branches+1 WHERE id=1;
END;
CREATE TRIGGER admission_branch_delete AFTER DELETE ON branches BEGIN
 UPDATE admission_counts SET branches=branches-1 WHERE id=1;
END;
CREATE TRIGGER admission_checkpoint_insert AFTER INSERT ON checkpoints BEGIN
 UPDATE admission_counts SET checkpoints=checkpoints+1 WHERE id=1;
END;
CREATE TRIGGER admission_checkpoint_delete AFTER DELETE ON checkpoints BEGIN
 UPDATE admission_counts SET checkpoints=checkpoints-1 WHERE id=1;
END;
CREATE TRIGGER admission_object_insert AFTER INSERT ON runtime_objects BEGIN
 UPDATE admission_counts SET objects=objects+1, object_bytes=object_bytes+NEW.byte_len WHERE id=1;
END;
CREATE TRIGGER admission_object_delete AFTER DELETE ON runtime_objects BEGIN
 UPDATE admission_counts SET objects=objects-1, object_bytes=object_bytes-OLD.byte_len WHERE id=1;
END;
CREATE TRIGGER admission_object_resize AFTER UPDATE OF byte_len ON runtime_objects BEGIN
 UPDATE admission_counts SET object_bytes=object_bytes-OLD.byte_len+NEW.byte_len WHERE id=1;
END;
CREATE TRIGGER admission_edge_insert AFTER INSERT ON runtime_object_edges BEGIN
 UPDATE admission_counts SET object_bytes=object_bytes+37 WHERE id=1;
END;
CREATE TRIGGER admission_edge_delete AFTER DELETE ON runtime_object_edges BEGIN
 UPDATE admission_counts SET object_bytes=object_bytes-37 WHERE id=1;
END;
CREATE TRIGGER admission_tombstone_insert AFTER INSERT ON delete_tombstones BEGIN
 UPDATE admission_counts SET tombstones=tombstones+1 WHERE id=1;
END;
CREATE TRIGGER admission_tombstone_delete AFTER DELETE ON delete_tombstones BEGIN
 UPDATE admission_counts SET tombstones=tombstones-1 WHERE id=1;
END;
