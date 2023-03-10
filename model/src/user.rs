use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "datacache", derive(datacache::DataMarker))]
pub struct PartialUser {
    pub uid: Uuid,
    pub name: String,
    pub avatar_url: Option<String>,
    pub is_admin: bool,
    #[serde(skip)]
    pub password_change_date: time::OffsetDateTime,
}
