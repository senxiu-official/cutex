ALTER TABLE pa_requests ADD COLUMN prepare_actor TEXT;
ALTER TABLE pa_requests ADD COLUMN prepare_action TEXT;
ALTER TABLE pa_requests ADD COLUMN prepare_digest TEXT;
ALTER TABLE pa_requests ADD COLUMN prepare_origin TEXT
    CHECK (prepare_origin IS NULL OR length(CAST(prepare_origin AS BLOB)) <= 2048);
CREATE UNIQUE INDEX pa_requests_prepare_action ON pa_requests(prepare_actor, prepare_action)
    WHERE prepare_action IS NOT NULL;
CREATE INDEX pa_requests_open ON pa_requests(assistant)
    WHERE state IN ('awaitingOwner', 'clarification');

ALTER TABLE pa_notices ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pa_notices ADD COLUMN retry_at INTEGER NOT NULL DEFAULT 0;
CREATE INDEX pa_notices_due ON pa_notices(retry_at, seq);

CREATE TABLE pa_reminder_cancellations (session TEXT PRIMARY KEY NOT NULL);
INSERT INTO pa_reminder_cancellations(session)
    SELECT session FROM pa_assistants WHERE session IS NOT NULL AND json_extract(body, '$.enabled') = 0;
PRAGMA user_version = 2;
