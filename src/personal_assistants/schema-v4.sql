UPDATE pa_assistants AS a SET body=json_set(body,'$.config.projectRefs',json((
    SELECT json_group_array(json_object('instanceId',json_extract(a.body,'$.config.homeInstanceId'),'projectId',p.value))
    FROM json_each(a.body,'$.config.projectIds') AS p
)), '$.coordinationProjectRef',json_object('instanceId',json_extract(body,'$.config.homeInstanceId'),
    'projectId',json_extract(body,'$.coordinationProjectId')));
UPDATE pa_requests AS r SET body=json_set(r.body,'$.projectRef',json_object(
    'instanceId',json_extract(a.body,'$.config.homeInstanceId'),'projectId',r.project))
FROM pa_assistants AS a WHERE a.id=r.assistant;
ALTER TABLE pa_requests ADD COLUMN due_ms INTEGER;
CREATE INDEX pa_requests_due ON pa_requests(assistant,due_ms,seq) WHERE due_ms IS NOT NULL;
CREATE INDEX pa_requests_source_state ON pa_requests(assistant,state,seq);
CREATE INDEX pa_requests_project_history ON pa_requests(json_extract(body,'$.projectRef.instanceId'),project,seq);
CREATE INDEX pa_requests_project_state ON pa_requests(json_extract(body,'$.projectRef.instanceId'),project,state,seq);
CREATE INDEX pa_requests_director_history ON pa_requests(director,seq);
CREATE INDEX pa_requests_director_state ON pa_requests(director,state,seq);
CREATE INDEX pa_requests_seat_history ON pa_requests(director,json_extract(body,'$.director.epoch'),seq);
CREATE TABLE pa_calendar_sources (
    assistant TEXT PRIMARY KEY REFERENCES pa_assistants(id), revision INTEGER NOT NULL,
    source TEXT, observed_at TEXT
);
CREATE TABLE pa_calendar_events (
    assistant TEXT NOT NULL REFERENCES pa_calendar_sources(assistant), event_id TEXT NOT NULL,
    start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL, body TEXT NOT NULL, PRIMARY KEY(assistant,event_id)
);
CREATE INDEX pa_calendar_agenda ON pa_calendar_events(assistant,start_ms,event_id);
PRAGMA user_version=4;
