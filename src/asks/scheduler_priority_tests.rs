    #[test]
    fn ordinary_send_passes_unstarted_delegation_but_explicit_queue_is_fifo() {
        use crate::management::v2::user_input::{OwnerStart, parse_user_input_submit_params};
        let (store, dir) = temp_store("owner-send-priority");
        let queue = store.queue_repository().unwrap();
        let ask = store.with_state(|state| state.create_ask(new_ask("ask", "work"), Utc::now()).unwrap().0.ask_id).unwrap();
        assert_eq!(queue.owner_send_start_with_reason(TARGET, None, true).unwrap().0, OwnerStart::Ready);
        assert_eq!(queue.owner_start(TARGET, None).unwrap(), OwnerStart::Blocked);
        let params = parse_user_input_submit_params(&json!({
            "clientUserMessageId":"send", "origin":{"kind":"other","clientId":"test"},
            "strategy":"auto", "input":[{"type":"text","text":"owner"}]
        })).unwrap();
        queue.enqueue(TARGET, "thread", "send", &params).unwrap();
        let admission = queue.admission(TARGET).unwrap();
        assert_eq!(admission.items[0].kind, "owner");
        assert_eq!(admission.items[1].id, ask);
        let explicit = queued_owner(&store, "explicit");
        let admission = queue.admission(TARGET).unwrap();
        assert_eq!(admission.items[2].id, explicit.queue_id);
        // Retry/reopen preserves order; fresh sends do not pass earlier owners.
        queue.enqueue(TARGET, "thread", "send", &params).unwrap();
        assert_eq!(queue.admission(TARGET).unwrap().items, admission.items);
        assert_eq!(queue.owner_send_start_with_reason(TARGET, None, true).unwrap().0, OwnerStart::Blocked);
        let _ = std::fs::remove_dir_all(dir);
    }

