-- Reserved actors and owner-only operations share an independent audit budget.
ALTER TABLE pa_audit ADD COLUMN owner_action INTEGER GENERATED ALWAYS AS
    (actor IN ('owner', 'system') OR verb IN ('create', 'bind', 'configure', 'confirm', 'cancel')) VIRTUAL;
CREATE INDEX pa_audit_history_class ON pa_audit(assistant, owner_action, seq DESC);
PRAGMA user_version = 3;
