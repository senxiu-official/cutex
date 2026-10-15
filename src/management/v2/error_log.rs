//! Bounded local diagnostic log. Never records request bodies or credentials.
use std::{cell::RefCell, io::Write, sync::Mutex};
use serde_json::{json, Value};
thread_local! { static REQUEST:RefCell<Value>=const {RefCell::new(Value::Null)}; }
static LOCK:Mutex<()>=Mutex::new(());
pub struct Scope(Value);
impl Drop for Scope {fn drop(&mut self){REQUEST.with(|r|*r.borrow_mut()=self.0.take());}}
pub fn scope(request:&crate::http::server::SimpleHttpRequest)->Scope {
    let body:Value=serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let mut ids=serde_json::Map::new();
    for key in ["action_id","actionId","requestId","cutex_session_id","session_id","threadId","native_id"] {
        if let Some(value)=body.get(key).and_then(Value::as_str){ids.insert(key.into(),json!(value.chars().take(200).collect::<String>()));}
    }
    let context=json!({"method":request.method,"path":request.path.split('?').next().unwrap_or("").chars().take(512).collect::<String>(),"ids":ids});
    Scope(REQUEST.with(|r|r.replace(context)))
}
pub fn record(status:u16,request_id:Option<&str>,code:&str,message:&str)->Option<String> {
    let event_id=uuid::Uuid::new_v4().to_string();
    let result=(||->anyhow::Result<()> {
        let _lock=LOCK.lock().map_err(|_|anyhow::anyhow!("diagnostic lock poisoned"))?;
        let dir=crate::config::paths::runtime_dir()?.join("service-logs");std::fs::create_dir_all(&dir)?;
        let path=dir.join("management-errors.jsonl");
        if std::fs::metadata(&path).is_ok_and(|m|m.len()>2*1024*1024) {
            let old=path.with_extension("jsonl.1");if old.exists(){std::fs::remove_file(&old)?;}std::fs::rename(&path,old)?;
        }
        let mut options=std::fs::OpenOptions::new();options.create(true).append(true);
        #[cfg(unix)] {use std::os::unix::fs::OpenOptionsExt;options.mode(0o600);}
        let mut f=options.open(path)?;
        let entry=json!({"time":chrono::Utc::now().to_rfc3339(),"eventId":event_id,"requestId":request_id,"httpStatus":status,"code":code,"message":message.chars().take(4096).collect::<String>(),"request":REQUEST.with(|r|r.borrow().clone())});
        writeln!(f,"{}",serde_json::to_string(&entry)?)?;Ok(())
    })();
    match result {Ok(())=>Some(event_id),Err(e)=>{eprintln!("Management diagnostic log unavailable: {e}");None}}
}
