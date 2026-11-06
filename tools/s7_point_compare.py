"""Actual exact-previous/new point reads while only S7 history grows 10x.

Run through cargo-slot (10x). All other fixture state and all 166 active sessions
stay fixed. Timings are supporting evidence, and binary optimization profiles
differ; compare each binary's growth ratio rather than absolute old/new speed.
"""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import statistics
import sqlite3
from contextlib import closing
import sys
import time
import urllib.request
import uuid


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--previous',type=Path,required=True)
    p.add_argument('--candidate',type=Path,required=True)
    p.add_argument('--gate-dir',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True)
    a=p.parse_args()
    a.output.mkdir(parents=True)
    sys.path.insert(0,str(a.gate_dir.resolve()))
    from fixture import generate
    import run
    from s7_scale_gate import seed
    class Instance(run.Instance):
        def request(self,path,body=None):
            request=urllib.request.Request(f'http://127.0.0.1:{self.mp}{path}',
                data=None if body is None else json.dumps(body).encode(),
                headers={'Authorization':f'Bearer {self.token}','Content-Type':'application/json'})
            started=time.perf_counter()
            with self.opener.open(request,timeout=180) as response:
                payload=response.read()
            value=json.loads(payload)
            if isinstance(value,dict) and value.get('error'):
                raise RuntimeError(value['error'])
            return dict(ms=(time.perf_counter()-started)*1000,response_bytes=len(payload),result=value)
    report=dict(synthetic=True,only_s7_history_grows=True,active_sessions=166,cases={},homes=[],executables={
        label:dict(path=str(exe),sha256=hashlib.file_digest(exe.open('rb'),'sha256').hexdigest())
        for label,exe in [('previous',a.previous),('candidate',a.candidate)]})
    def save():
        (a.output/'results.json').write_text(json.dumps(report,indent=2))
    for scale in (1,10):
        for label,exe in [('previous',a.previous),('candidate',a.candidate)]:
            home=Path.home()/('.scale-gate-'+uuid.uuid4().hex[:10])
            generate(home,.01,1,24977,24967)
            population=seed(home,history_multiplier=scale)
            report['homes'].append(str(home))
            out=a.output/f'{label}-{scale}x'
            out.mkdir()
            instance=Instance(exe.resolve(),home,out,24977,24967)
            path='/v2/sessions/cutex.scale-00000'
            try:
                instance.start()
                instance.request(path)
                if label=='candidate':
                    deadline=time.monotonic()+5
                    while not any(row.get('kind')=='request' and row.get('path')==path for row in run.metrics(out/'management.jsonl')):
                        if time.monotonic()>deadline: raise RuntimeError('warm request counter missing')
                        time.sleep(.01)
                before=len([row for row in run.metrics(out/'management.jsonl') if row.get('kind')=='request' and row.get('path')==path])
                replies=[instance.request(path) for _ in range(15)]
                if label=='candidate':
                    deadline=time.monotonic()+5
                    while len([row for row in run.metrics(out/'management.jsonl') if row.get('kind')=='request' and row.get('path')==path])<before+15:
                        if time.monotonic()>deadline: raise RuntimeError('point request counters missing')
                        time.sleep(.01)
                for reply in replies:
                    value=reply['result']
                    assert value['cutexSessionId']=='cutex.scale-00000'
                    assert value['activity']['revision']==1
                    assert value['activity']['lastConversationAt']=='2026-11-04T01:02:03Z'
                    assert value['focus']['revision']==1
                values=sorted(row['ms'] for row in replies)
                # Request counters are available only in the instrumented new
                # binary. The old code's full JSON read size is separately
                # recorded as a source-derived lower bound, not an OS trace.
                counters=[row['parse_bytes'] for row in run.metrics(out/'management.jsonl')
                    if row.get('kind')=='request' and row.get('path')==path][before:]
                update=instance.request('/v2/sessions/cutex.scale-00000/cutex/requests',dict(
                    requestId='s7-point-'+uuid.uuid4().hex,method='cutex/focus/set',
                    params=dict(expectedRevision=1,owner='mobile',mobileMuted=True,source='s7-point-comparison')))
                assert update['result']['cutex']['result']['revision']==2
                projection_size={}
                if label=='previous':
                    projection_size['whole_json_bytes_written']=(home/'.cutex/runtime/management-v2/session-projection-state.json').stat().st_size
                else:
                    database=home/'.cutex/runtime/management-v2/session-projection.sqlite3'
                    with closing(sqlite3.connect(database.resolve().as_uri()+'?mode=ro',uri=True)) as db:
                        projection_size['selected_row_value_bytes']=db.execute("SELECT length(data) FROM session_rows WHERE id='cutex.scale-00000'").fetchone()[0]
                report['cases'][f'{label}-{scale}x']=dict(population=population,requests=len(replies),
                    median_ms=statistics.median(values),p95_ms=values[-1],private_bytes_after=instance.memory(),
                    response_bytes=sorted({row['response_bytes'] for row in replies}),
                    focus_update_ms=update['ms'],projection_update=projection_size,
                    parse_bytes_per_request=counters,
                    predecessor_activity_read_lower_bound=population['session-activity']['source_bytes'] if label=='previous' else None)
            finally:
                instance.close()
                save()
    for label in ('previous','candidate'):
        small,large=(report['cases'][f'{label}-{scale}x'] for scale in (1,10))
        report.setdefault('growth',{})[label]=dict(median_ratio=large['median_ms']/small['median_ms'],
            private_bytes_delta=large['private_bytes_after']-small['private_bytes_after'])
    small=report['cases']['candidate-1x']['parse_bytes_per_request']
    large=report['cases']['candidate-10x']['parse_bytes_per_request']
    assert len(small)==15 and len(large)==15,'candidate request counters missing'
    # The same bounded small-configuration allowance as the pinned route gate;
    # mandatory repository counters separately require exact work equality.
    report['point_parse_budget']=max(small)*1.5+16*1024
    assert max(large)<=report['point_parse_budget'],'S7-only growth exceeded point parse budget'
    report['result']='PASS'
    save()
    print(json.dumps(report,indent=2))


if __name__=='__main__':
    main()
