//! Exercises the real request-claim + response settlement boundary, without a
//! daemon, credentials or runtime. Included by the shared server wiring patch.
use super::*;
use std::io::Read;

#[test]
fn model_overload_retry_fault_settles_claim_and_allows_safety_stop() {
    use crate::management::v2::{model_overload_retry as retry, request_idempotency::{BeginRequest, RequestIdempotencyRepository}};
    for (reason, expected_status) in [("ownerStop",200),("ownerMessage",503)] {
        let dir=std::env::temp_dir().join(format!("retry-claim-{}",uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let journal=retry::Store::at(dir.join("retry.json"));
        std::fs::write(dir.join("retry.json"),b"corrupt journal").unwrap();
        let requests=RequestIdempotencyRepository::open(dir.join("requests"),"private-test".into()).unwrap();
        let body=json!({"method":reason});
        let BeginRequest::Forward(claim)=requests.begin("private-session","request-1",&body).unwrap() else {panic!("new claim")};
        let dispatched=std::cell::Cell::new(0);
        let operation=retry::dispatch_after_cancel_result(Some(&journal),reason,journal.snapshot().map(|_|false),|| {
            dispatched.set(dispatched.get()+1); Ok(json!({"stopped":true}))
        });
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut reader=TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut stream,_)=listener.accept().unwrap();
        match operation {
            Ok(result)=>{
                requests.complete(&claim,200,"OK",result.clone()).unwrap();
                write_json_response(&mut stream,200,"OK",&result).unwrap();
            }
            Err(error)=>write_cutex_operation_error(&mut stream,&requests,&claim,"request-1",error).unwrap(),
        }
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response=String::new();reader.read_to_string(&mut response).unwrap();
        assert!(response.contains(&expected_status.to_string()));
        let BeginRequest::Completed(receipt)=requests.begin("private-session","request-1",&body).unwrap() else {panic!("claim left unresolved")};
        assert_eq!(receipt.status,expected_status);
        assert_eq!(dispatched.get(),if reason=="ownerStop"{1}else{0});
        assert!(!journal.automatic_enabled());
    }
}
