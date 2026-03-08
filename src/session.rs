use anyhow::Result;
use anyhow::anyhow;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::VecDeque;

#[derive(Debug)]
pub(crate) struct SessionStore {
    messages_by_response_id: HashMap<String, Vec<Value>>,
    insertion_order: VecDeque<String>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self {
            messages_by_response_id: HashMap::new(),
            insertion_order: VecDeque::new(),
        }
    }
}

impl SessionStore {
    const MAX_ENTRIES: usize = 1024;

    pub(crate) fn get_messages(&self, response_id: &str) -> Option<Vec<Value>> {
        self.messages_by_response_id.get(response_id).cloned()
    }

    pub(crate) fn insert_messages(&mut self, response_id: String, messages: Vec<Value>) {
        if self.messages_by_response_id.contains_key(&response_id) {
            self.insertion_order.retain(|id| id != &response_id);
        }
        self.messages_by_response_id
            .insert(response_id.clone(), messages);
        self.insertion_order.push_back(response_id);

        while self.messages_by_response_id.len() > Self::MAX_ENTRIES {
            let Some(oldest_id) = self.insertion_order.pop_front() else {
                break;
            };
            self.messages_by_response_id.remove(&oldest_id);
        }
    }
}

pub(crate) fn previous_response_id_for_request(request: &Value) -> Option<&str> {
    request
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

pub(crate) fn resolve_previous_messages_for_request(
    request: &Value,
    sessions: &SessionStore,
) -> Result<Option<Vec<Value>>> {
    let Some(previous_response_id) = previous_response_id_for_request(request) else {
        return Ok(None);
    };

    let Some(messages) = sessions.get_messages(previous_response_id) else {
        return Err(anyhow!(
            "unknown `previous_response_id`: {previous_response_id}"
        ));
    };
    Ok(Some(messages))
}

pub(crate) fn merge_previous_messages(
    upstream_payload: &mut Value,
    mut previous_messages: Vec<Value>,
) -> Result<()> {
    let obj = upstream_payload
        .as_object_mut()
        .ok_or_else(|| anyhow!("upstream chat payload must be an object"))?;
    let messages = obj
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow!("upstream chat payload is missing `messages` array"))?;
    if previous_messages.is_empty() {
        return Ok(());
    }
    let current_messages = std::mem::take(messages);
    previous_messages.extend(current_messages);
    *messages = previous_messages;
    Ok(())
}
