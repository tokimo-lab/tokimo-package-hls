use super::helpers::BROWSER_DIRECT_PLAY_CONTAINERS;

/// Returns true if the container format needs transcoding.
pub fn needs_container_transcode(path: &str, client_containers: &[String]) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    if ext.is_empty() {
        return false;
    }
    if client_containers.is_empty() {
        !BROWSER_DIRECT_PLAY_CONTAINERS.contains(ext.as_str())
    } else {
        !client_containers.iter().any(|c| c == &ext)
    }
}

/// Returns the container extension if it needs transcoding, or None.
pub fn container_transcode_reason(path: &str, client_containers: &[String]) -> Option<String> {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    if ext.is_empty() {
        return None;
    }
    let supported = if client_containers.is_empty() {
        BROWSER_DIRECT_PLAY_CONTAINERS.contains(ext.as_str())
    } else {
        client_containers.iter().any(|c| c == &ext)
    };
    if supported {
        None
    } else {
        Some(format!("ContainerNotSupported ({ext})"))
    }
}
