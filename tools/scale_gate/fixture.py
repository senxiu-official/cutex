"""Synthetic EVA-shaped fixtures. Never opens live content; optional inventory uses stat only."""
import argparse, datetime, hashlib, json, os, pathlib, secrets, socket, subprocess

def encode(x): return json.dumps(x,separators=(',',':')).encode()
def write(p,x):
    p.parent.mkdir(parents=True,exist_ok=True); p.write_bytes(encode(x))
    if os.name!='nt': p.chmod(0o600)
def secure(p):
    p.mkdir(parents=True,exist_ok=True)
    if os.name=='nt':
        sid=subprocess.check_output(['whoami','/user','/fo','csv','/nh'],text=True).strip().split(',')[-1].strip('"')
        subprocess.run(['icacls',str(p),'/inheritance:r','/grant:r',f'*{sid}:(OI)(CI)F'],check=True,stdout=subprocess.DEVNULL)
    else: p.chmod(0o700)

def generate(home,scale=1,history=1,management_port=24870,bus_port=24860,inventory=None,anchor=None):
    if scale<=0 or history<1: raise ValueError('scale must be positive and history >= 1')
    home=home.resolve()
    if home.exists(): raise ValueError(f'fixture must be fresh: {home}')
    secure(home); (home/'.cutex-test-private-home').write_text('synthetic scale fixture\n')
    root=home/'.cutex'; secure(root); runtime=root/'runtime'; runtime.mkdir()
    # Typed roster timestamps require canonical RFC3339 (UTC Z, AutoSi), not +00:00.
    now=datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace('+00:00','Z'); host=socket.gethostname()
    write(root/'config.json',dict(agent_bus_enabled=True,agent_bus_port=bus_port,management_port=management_port,agent_bus_token=secrets.token_hex(32),management_api_token=secrets.token_hex(32)))
    sessions={}
    for i in range(166+int(166*(history-1))):
        sid=f'cutex.scale-{i:05}'
        sessions[sid]=dict(cutex_session_id=sid,host_id=host,cwd=str(home),created_at=now,updated_at=now,formal_agent_name=f'Scale-{i:05}',note='synthetic session metadata '*30,lifecycle='active' if i<166 else 'retired',runtime_generation=0,agent_enabled=True,registration_class='persistent',exposed_to_backend=True,agent_groups=['scale'])
        if i>=166: sessions[sid]['retiredAt']=now
    write(root/'cutex-sessions.json',dict(sessions=sessions))
    identities={}
    for i,sid in enumerate(sessions):
        identities[sid]=dict(queueRevision=1,identities={f'input-{j}':dict(canonicalSha256=hashlib.sha256(f'{i}:{j}'.encode()).hexdigest(),firstSeenAt=now) for j in range(max(1,int(60*scale)))})
    selected='cutex.scale-00000'
    identities[selected].update(items=[dict(managementRequestId='scale-queue-request',threadId='scale-unbound-thread',item=dict(queueId='queue-scale-sentinel',clientUserMessageId='input-scale-sentinel',origin=dict(kind='other',clientId='scale-gate'),input=[dict(type='text',text='Synthetic queued sentinel')],position=1,revision=1,state='queued',createdAt=now,updatedAt=now))],pause=dict(pausedAt=now,reason='manual'))
    write(runtime/'management-v2'/'user-input-state.json',dict(version=1,sessions=identities))
    # Fixed live page, unrelated completed history grows independently.
    asks={}; answers={}
    for i in range(max(1,int(1000*scale*history))):
        aid=f'ask-scale-{i:06}'; text='Synthetic delegation brief. '*75
        asks[aid]=dict(askId=aid,actionId=f'action-{i}',requestSha256='0'*64,requester=dict(kind='owner'),targetCutexSessionId='cutex.scale-00000',title=f'Synthetic {i}',body=dict(text=text,textBytes=len(text),textSha256=hashlib.sha256(text.encode()).hexdigest()),options={},seq=i+1,execution='answered',closed='completed',closedAt=now,createdAt=now,updatedAt=now,revision=1)
        if i>=10: asks[aid]['targetCutexSessionId']='cutex.scale-00001'
        answer=('Synthetic answer %06d. '%i)*700
        digest=hashlib.sha256(answer.encode()).hexdigest(); artifact=f'sha256:{digest}'
        artifact_path=runtime/'asks'/'artifacts'/f'{digest}.blob'; artifact_path.parent.mkdir(parents=True,exist_ok=True); artifact_path.write_text(answer)
        answer_id=f'answer-{i}'; asks[aid]['answerId']=answer_id
        answers[answer_id]=dict(answerId=answer_id,askId=aid,attempt=1,revision=1,status='completed',artifactId=artifact,bytes=len(answer),metadata=dict(nativeDiff='Synthetic changed line\n'*600),createdAt=now,preview=answer[:300])
    closed_count=len(asks)
    for i in range(4):
        aid=f'ask-open-{i}'
        asks[aid]=dict(asks['ask-scale-000000'],askId=aid,actionId=f'open-action-{i}',targetCutexSessionId='cutex.scale-00165',seq=len(asks)+1,execution='pendingReview',closed=None,closedAt=None)
        asks[aid].pop('answerId',None)
    pauses={sid:dict(cutexSessionId=sid,by='owner',since=now,origin='ownerPause') for sid in sessions}
    write(runtime/'asks'/'asks-v1.json',dict(revision=1,nextSeq=len(asks)+1,asks=asks,answers=answers,ownerOffline=list(sessions),pauses=pauses))
    actions={}
    for i in range(max(1,int(11500*scale*history))):
        action=f'synthetic-action-{i}'
        actions[action]=dict(action_id=action,request_sha256='0'*64,operation='query_managed',project_id='scale-project',caller_cutex_session='cutex.scale-00000',phase='complete',created_at=now,updated_at=now)
    agents={sid:dict(project_id='scale-project',created_by_director_session=selected,cutex_session_id=sid,native_session_id=f'synthetic-unbound-{i}',spec=dict(name=s['formal_agent_name'],cwd=str(home),profile='scale-api',runtime_backend='host',model='synthetic-model',reasoning='medium',permissions='read-only',approval_policy='never',sandbox_mode='read-only',groups=['scale']),created_at=now,retired_at=now if i>=166 else None) for i,(sid,s) in enumerate(sessions.items())}
    write(runtime/'agent-management/v1/agent-management-v1.json',dict(schema='cutex/agent-management-store/v1',store_revision=1,projects={'scale-project':dict(project_id='scale-project',authorized_director_session=selected,authority_epoch=1,updated_at=now)},agents=agents,actions=actions,authority_receipts={},failure_events={}))
    # snapshot_shared treats a missing lock as an absent roster, even with JSON present.
    (runtime/'agent-management/v1/agent-management-v1.lock').touch()
    notifications={}
    for i in range(max(1,int(56*scale*history))):
        nid=f'notice-{i}'; notifications[nid]=dict(notificationId=nid,agentId='cutex.scale-00000',agentName='Scale',text='Synthetic notification. '*40,level='info',requestedLevel='info',downgraded=None,muted=False,channels=[],needsReply=False,delegation=None,job=None,turnId=None,state='dismissed',createdAt=now,acknowledgedAt=None,reply=None,closedAt=now)
    notifications['notice-scale-sentinel']=dict(notifications['notice-0'],notificationId='notice-scale-sentinel',text='Synthetic pending sentinel',state='open',needsReply=True,closedAt=None)
    write(runtime/'owner-notify'/'owner-notify-v1.json',dict(revision=1,notifications=notifications))
    messages={}; count=max(1,int(1846*scale*history)); per=max(128,int(35*1024**2*scale*history/count)-450)
    for i in range(count):
        mid=f'bus-scale-{i}'; messages[mid]=dict(ownerCutexSessionId='cutex.scale-00000',fromRuntimeAgentId=None,toRuntimeAgentId=None,updatedAt=now,snapshot=dict(messageId=mid,fromCutexSessionId=None,toCutexSessionId='cutex.scale-00000',deliveryMode='after_turn',content='x'*per,state='delivered',error=None))
    old=(datetime.datetime.now(datetime.timezone.utc)-datetime.timedelta(hours=2)).isoformat()
    messages['bus-scale-sentinel']=dict(ownerCutexSessionId=selected,fromRuntimeAgentId=None,toRuntimeAgentId=None,updatedAt=old,queuedAt=old,snapshot=dict(messageId='bus-scale-sentinel',fromCutexSessionId=None,toCutexSessionId='cutex.scale-00001',deliveryMode='after_turn',content='Synthetic held bus sentinel',state='stale_held',error=None))
    write(runtime/'management-v2'/'agent-bus-message-state.json',dict(version=2,messages=messages))
    # Native/external split mirrors ~70/90 MiB of the audited ~160 MiB month.
    # One selected local day row; growth belongs to another day/profile.
    day=datetime.date.fromisoformat(anchor) if anchor else datetime.date.today()
    unrelated=day.replace(day=1 if day.day!=1 else 2)
    timestamp=lambda date: datetime.datetime.combine(date,datetime.time(12)).astimezone().isoformat()
    usage_rows=0
    for source,mib in [('native',70),('external',90)]:
        d=runtime/'management-v2'/('usage-ledger' if source=='native' else 'usage-ledger-external/scale-external'); d.mkdir(parents=True,exist_ok=True)
        path=d/f'{day:%Y-%m}.jsonl'; target=int(mib*1024**2*scale*history)
        with path.open('wb') as f:
            i=0
            while f.tell()<target:
                selected_sample=i==0
                row=dict(kind='usage',entryId=f'{source}-{i}',eventId=f'event-{source}-{i}',observedAt=timestamp(day if selected_sample else unrelated),hostId='scale-host',cutexSessionId='cutex.scale-00000',threadId='scale-thread',turnId=f'{source}-turn-{i}',runtimeGeneration=1,profile='scale-api' if selected_sample else 'scale-unrelated',profileId='scale-api' if selected_sample else 'scale-unrelated',provider='anthropic',model='synthetic-model',tokenSource='native_last',tokens=dict(totalTokens=3,inputTokens=2,outputTokens=1),cumulativeTotal=dict(totalTokens=3,inputTokens=2,outputTokens=1))
                if source=='external': row['source']=dict(id='scale-external',kind='claude-code',billing='api')
                f.write(encode(row)+b'\n'); i+=1
            usage_rows+=i
    write(root/'accounts.json',dict(version=3,accounts=[dict(id='scale-api',name='scale-api',email=None,plan_type=None,last_used_at=None,cli_kind='claude',claude_account='ca-scale')],active_account_id=None))
    write(runtime/'claude'/'accounts.json',dict(version=1,accounts=[dict(id='ca-scale',label='Synthetic API',kind='api',createdAt=now,updatedAt=now)]))
    sizes={str(p.relative_to(root)):p.stat().st_size for p in root.rglob('*') if p.is_file()}
    receipt=dict(synthetic=True,fixture_version=2,anchor=str(day),scale=scale,inactive_history_multiplier=history,active_sessions=166,closed_delegations=closed_count,open_delegations=4,usage_rows=usage_rows,expected=dict(active_session_ids=[f'cutex.scale-{i:05}' for i in range(166)],host_match=selected,queue_id='queue-scale-sentinel',pending_notification='notice-scale-sentinel',bus_sentinel='bus-scale-sentinel',bus_history_rows=count,roster_actions=len(actions),roster_agents=len(agents),final_notifications=len(notifications)-1,usage_total=6),files=sizes,calibration='2026-11-03 audits; 35 MiB bus is historical stress target; current capture 14.06 MiB',management_port=management_port,bus_port=bus_port)
    if inventory:
        # Explicit allowlist, metadata only. No recursive live traversal or content read.
        allowed=['runtime/management-v2/agent-bus-message-state.json','runtime/management-v2/user-input-state.json','runtime/owner-notify/owner-notify-v1.json','runtime/asks/asks-v1.json','cutex-sessions.sqlite3']
        receipt['live_metadata']={name:(inventory/name).stat().st_size for name in allowed if (inventory/name).is_file()}
    write(home/'fixture.json',receipt); return receipt

if __name__=='__main__':
    p=argparse.ArgumentParser(); p.add_argument('--home',type=pathlib.Path,required=True); p.add_argument('--scale',type=float,default=1); p.add_argument('--history',type=int,default=1); p.add_argument('--inventory',type=pathlib.Path); a=p.parse_args()
    print(json.dumps(generate(a.home,a.scale,a.history,inventory=a.inventory),indent=2))
