//! Byte permits live as long as any HTTP/B10/cache reference to a body does.
//! Eviction must not release a permit while a slow response still holds it.
use serde_json::Value;
use std::{
    ops::Deref,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
};
static BYTES: OnceLock<Arc<AtomicUsize>> = OnceLock::new();
fn shared_counter() -> &'static Arc<AtomicUsize> {
    BYTES.get_or_init(|| Arc::new(AtomicUsize::new(0)))
}
pub struct Body {
    pub value: Arc<Value>,
    /// Only owner-turn-shot results retain this validated compressed image.
    /// Its bytes share this body's permit, including while readers hold an Arc.
    pub decoded_image: Option<super::images::ValidImage>,
    bytes: usize,
    counter: Arc<AtomicUsize>,
}
impl Body {
    pub fn new(value: Value, bytes: usize, limit: usize) -> Option<Arc<Self>> {
        Self::new_with_counter(value, bytes, limit, Arc::clone(shared_counter()))
    }
    pub fn new_with_image(
        value: Value,
        bytes: usize,
        limit: usize,
        image: Option<super::images::ValidImage>,
    ) -> Option<Arc<Self>> {
        Self::new_image_counter(value, bytes, limit, image, Arc::clone(shared_counter()))
    }
    fn new_with_counter(
        value: Value,
        bytes: usize,
        limit: usize,
        counter: Arc<AtomicUsize>,
    ) -> Option<Arc<Self>> {
        Self::new_image_counter(value, bytes, limit, None, counter)
    }
    fn new_image_counter(
        value: Value,
        bytes: usize,
        limit: usize,
        image: Option<super::images::ValidImage>,
        counter: Arc<AtomicUsize>,
    ) -> Option<Arc<Self>> {
        let bytes = bytes.checked_add(image.as_ref().map_or(0, |image| image.bytes.len()))?;
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                n.checked_add(bytes).filter(|total| *total <= limit)
            })
            .ok()?;
        Some(Arc::new(Self {
            value: Arc::new(value),
            decoded_image: image,
            bytes,
            counter,
        }))
    }
    pub fn total() -> usize {
        shared_counter().load(Ordering::SeqCst)
    }
}
impl Deref for Body {
    type Target = Value;
    fn deref(&self) -> &Value {
        self.value.as_ref()
    }
}
impl Drop for Body {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_retained_validated_image_shares_byte_permit_until_last_body_reference() {
        let counter = Arc::new(AtomicUsize::new(0));
        let image = || super::super::images::ValidImage {
            bytes: vec![1; 6],
            mime: "image/png",
            width: 1,
            height: 1,
        };
        assert!(
            Body::new_image_counter(Value::Null, 5, 10, Some(image()), Arc::clone(&counter))
                .is_none()
        );
        let body = Body::new_image_counter(Value::Null, 4, 10, Some(image()), Arc::clone(&counter))
            .unwrap();
        let reader = Arc::clone(&body);
        drop(body);
        assert_eq!(counter.load(Ordering::SeqCst), 10);
        assert_eq!(reader.decoded_image.as_ref().unwrap().bytes.len(), 6);
        drop(reader);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn browser_retained_permit_includes_inflight_response() {
        let counter = Arc::new(AtomicUsize::new(0));
        let body =
            Body::new_with_counter(serde_json::json!({}), 10, 10, Arc::clone(&counter)).unwrap();
        let response = Arc::clone(&body);
        drop(body);
        assert_eq!(counter.load(Ordering::SeqCst), 10);
        assert!(
            Body::new_with_counter(serde_json::json!({}), 1, 10, Arc::clone(&counter)).is_none()
        );
        drop(response);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn browser_retained_distinct_bodies_share_one_budget() {
        let counter = Arc::new(AtomicUsize::new(0));
        let first =
            Body::new_with_counter(serde_json::json!({}), 6, 10, Arc::clone(&counter)).unwrap();
        let second =
            Body::new_with_counter(serde_json::json!({}), 4, 10, Arc::clone(&counter)).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 10);
        assert!(
            Body::new_with_counter(serde_json::json!({}), 1, 10, Arc::clone(&counter)).is_none()
        );
        drop(first);
        let next =
            Body::new_with_counter(serde_json::json!({}), 6, 10, Arc::clone(&counter)).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 10);
        drop(second);
        drop(next);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }
}
