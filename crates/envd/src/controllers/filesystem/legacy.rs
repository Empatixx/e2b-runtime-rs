use crate::proto::filesystem::__buffa::oneof::watch_dir_response::Event;
use crate::proto::filesystem::{EntryInfo, FilesystemEvent, WatchDirResponse};

pub fn entry(info: &EntryInfo) -> EntryInfo {
    EntryInfo {
        name: info.name.clone(),
        r#type: info.r#type,
        path: info.path.clone(),
        ..Default::default()
    }
}

pub fn event(event: &FilesystemEvent) -> FilesystemEvent {
    FilesystemEvent {
        name: event.name.clone(),
        r#type: event.r#type,
        ..Default::default()
    }
}

pub fn watch_response(response: WatchDirResponse) -> WatchDirResponse {
    let event = match response.event {
        Some(Event::Filesystem(filesystem)) => Some(Event::Filesystem(Box::new(event(&filesystem)))),
        Some(Event::Start(_)) => Some(Event::Start(Box::default())),
        Some(Event::Keepalive(_)) => Some(Event::Keepalive(Box::default())),
        None => None,
    };
    WatchDirResponse {
        event,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_fields_the_old_python_sdk_knew_survive() {
        let full = EntryInfo {
            name: "a".to_owned(),
            path: "/a".to_owned(),
            size: 9,
            owner: "user".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            entry(&full),
            EntryInfo {
                name: "a".to_owned(),
                path: "/a".to_owned(),
                ..Default::default()
            }
        );
        let with_entry = FilesystemEvent {
            name: "a".to_owned(),
            entry: full.into(),
            ..Default::default()
        };
        assert!(event(&with_entry).entry.as_option().is_none());
    }
}
