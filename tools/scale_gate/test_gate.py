import json, os, pathlib, tempfile, unittest
from unittest.mock import patch
from run import compare, env_for, metrics, private_bytes, validate_response, audit_seed, CASES
from fixture import generate

class GateTests(unittest.TestCase):
    def row(self,parse=1000,memory=1000,latency=5):
        return dict(concurrency=2,parse_bytes_per_request=parse,transient_private_bytes=memory,latency_median_ms=latency)
    def test_reject_whole_month_parse(self):
        failures=compare({'day':self.row(16*1024**2)},{'day':self.row(160*1024**2)})
        self.assertEqual([f['metric'] for f in failures],['parse_bytes_per_request'])
    def test_reject_copy_without_new_parse(self):
        failures=compare({'cached':self.row()},{'cached':self.row(memory=100*1024**2)})
        self.assertEqual([f['metric'] for f in failures],['transient_private_bytes'])
    def test_reject_small_notification_document_scan(self):
        failures=compare({'pending':self.row(6144)},{'pending':self.row(76060)})
        self.assertEqual([f['metric'] for f in failures],['parse_bytes_per_request'])
    def test_bounded_work_passes(self):
        self.assertEqual(compare({'point':self.row()},{'point':self.row(1100,2000,8)}),[])
    def test_scrub_live_environment(self):
        with tempfile.TemporaryDirectory() as d, patch.dict(os.environ,{'CUTEX_HOME':'live','CUTEX_BIN':'live','CODEX_HOME':'live','ANTHROPIC_API_KEY':'secret'}):
            env=env_for(pathlib.Path(d),pathlib.Path(d)/'metrics')
            for key in ['CUTEX_HOME','CUTEX_BIN','CODEX_HOME','ANTHROPIC_API_KEY']: self.assertNotIn(key,env)
            self.assertEqual(env['CUTEX_TEST_PRIVATE_HOME'],d)
    def test_metrics_trailing_partial_is_not_corruption(self):
        with tempfile.TemporaryDirectory() as d:
            p=pathlib.Path(d)/'metrics'; p.write_text('{"kind":"total"}\n{"kind":')
            self.assertEqual(len(metrics(p)),1)
            p.write_text('{corrupt}\n')
            with self.assertRaises(json.JSONDecodeError): metrics(p)
    def test_memory_is_private_and_nonzero(self):
        self.assertGreater(private_bytes(os.getpid()),1024**2)
    def expected(self):
        return dict(active_session_ids=[f'cutex.scale-{i:05}' for i in range(166)],host_match='cutex.scale-00000',queue_id='queue-scale-sentinel',pending_notification='notice-scale-sentinel',bus_sentinel='bus-scale-sentinel',usage_total=6)
    def test_empty_http_success_rejected_for_every_route(self):
        for name,path,_ in CASES:
            with self.subTest(endpoint=name), self.assertRaises(RuntimeError):
                validate_response(path,{},self.expected(),'2026-10-30')
    def test_sessions_require_exact_visible_identities(self):
        rows=[dict(cutexSessionId=sid,visible=True) for sid in self.expected()['active_session_ids']]
        self.assertEqual(validate_response('/v2/sessions',dict(sessions=rows),self.expected(),'2026-10-30')['rows'],166)
        for bad in [rows[:-1],rows[:-1]+[rows[0]],[dict(r,visible=False) for r in rows]]:
            with self.subTest(rows=len(bad)), self.assertRaises(RuntimeError):
                validate_response('/v2/sessions',dict(sessions=bad),self.expected(),'2026-10-30')
    def test_host_requires_selected_match_and_decoded_roster(self):
        valid=dict(data=[dict(id='cutex.scale-00000',projectId='scale-project',kind='Agent',state='Offline')])
        self.assertEqual(validate_response('/v2/host/sessions',valid,self.expected(),'2026-10-30')['rows'],1)
        for rows in [[],[dict(valid['data'][0],projectId=None)],[dict(valid['data'][0],id='cutex.wrong')]]:
            with self.subTest(rows=rows), self.assertRaises(RuntimeError):
                validate_response('/v2/host/sessions',dict(data=rows),self.expected(),'2026-10-30')
    def test_quota_source_without_usage_evidence_fails(self):
        wrong=dict(source='claudeApi',nativeQuota=dict(claudeAccount=dict(id='ca-scale'),apiUsage=dict(last30Days=dict(total=0))))
        with self.assertRaises(RuntimeError): validate_response('/v2/profiles/scale-api/quota',wrong,self.expected(),'2026-10-30')
    def test_usage_requires_native_and_external_evidence(self):
        path='/v2/usage?from=2026-10-30&to=2026-10-30&groupBy=day'
        row=dict(rows=[{}],totals=dict(total=3),warnings=[])
        with self.assertRaises(RuntimeError): validate_response(path,row,self.expected(),'2026-10-30')
        for source in ['cutex','scale-external']:
            self.assertEqual(validate_response(path+'&source='+source,row,self.expected(),'2026-10-30')['total'],3)
    def test_seed_rejects_old_managed_flags_lock_and_timestamps(self):
        with tempfile.TemporaryDirectory() as d:
            home=pathlib.Path(d)/'seed'; generate(home,.01,anchor='2026-10-30')
            self.assertEqual(audit_seed(home)['visible_active_sessions'],166)
            path=home/'.cutex/cutex-sessions.json'; original=path.read_text()
            for field in ['agent_enabled','registration_class','exposed_to_backend']:
                data=json.loads(original); data['sessions']['cutex.scale-00000'].pop(field); path.write_text(json.dumps(data))
                with self.subTest(missing=field), self.assertRaisesRegex(RuntimeError,'hidden'): audit_seed(home)
            path.write_text(original)
            lock=home/'.cutex/runtime/agent-management/v1/agent-management-v1.lock'; lock.unlink()
            with self.assertRaisesRegex(RuntimeError,'lock'): audit_seed(home)
            lock.touch()
            path=lock.with_suffix('.json'); data=json.loads(path.read_text()); next(iter(data['actions'].values()))['created_at']='2026-11-04T00:00:00+00:00'; path.write_text(json.dumps(data))
            with self.assertRaisesRegex(RuntimeError,'noncanonical'): audit_seed(home)

if __name__=='__main__': unittest.main()
