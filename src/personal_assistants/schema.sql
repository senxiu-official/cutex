CREATE TABLE pa_assistants (
    id TEXT PRIMARY KEY, session TEXT UNIQUE, body TEXT NOT NULL
);
CREATE TABLE pa_actions (
    actor TEXT NOT NULL, action TEXT NOT NULL, digest TEXT NOT NULL, reply TEXT NOT NULL,
    PRIMARY KEY (actor, action)
);
CREATE TABLE pa_audit (
    seq INTEGER PRIMARY KEY, assistant TEXT NOT NULL REFERENCES pa_assistants(id),
    actor TEXT NOT NULL, action TEXT NOT NULL, verb TEXT NOT NULL, at TEXT NOT NULL, detail TEXT NOT NULL
);
CREATE INDEX pa_audit_by_assistant ON pa_audit(assistant, seq DESC);
CREATE TABLE pa_requests (
    seq INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, assistant TEXT NOT NULL REFERENCES pa_assistants(id),
    director TEXT, project TEXT NOT NULL, state TEXT NOT NULL, body TEXT NOT NULL, admission_seq INTEGER UNIQUE
);
CREATE INDEX pa_requests_by_source ON pa_requests(assistant, seq);
CREATE INDEX pa_requests_by_director ON pa_requests(director, admission_seq);
CREATE TABLE pa_notices (
    seq INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, recipient TEXT NOT NULL, message TEXT NOT NULL
);
CREATE TABLE pa_memory (
    assistant TEXT NOT NULL REFERENCES pa_assistants(id), key TEXT NOT NULL, revision INTEGER NOT NULL,
    content TEXT NOT NULL, source TEXT NOT NULL, PRIMARY KEY (assistant, key)
);
PRAGMA user_version = 1;
