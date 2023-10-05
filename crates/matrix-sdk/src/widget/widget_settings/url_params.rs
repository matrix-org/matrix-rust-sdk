use url::Url;
use urlencoding::encode;

pub static USER_ID: &str = "$matrix_user_id";
pub static ROOM_ID: &str = "$matrix_room_id";
pub static WIDGET_ID: &str = "$matrix_widget_id";
pub static AVATAR_URL: &str = "$matrix_avatar_url";
pub static DISPLAY_NAME: &str = "$matrix_display_name";
pub static LANGUAGE: &str = "$org.matrix.msc2873.client_language";
pub static CLIENT_THEME: &str = "$org.matrix.msc2873.client_theme";
pub static CLIENT_ID: &str = "$org.matrix.msc2873.client_id";
pub static DEVICE_ID: &str = "$org.matrix.msc2873.matrix_device_id";
pub static HOMESERVER_URL: &str = "$org.matrix.msc4039.matrix_base_url";

pub struct QueryProperties {
    pub(crate) widget_id: String,
    pub(crate) avatar_url: String,
    pub(crate) display_name: String,
    pub(crate) user_id: String,
    pub(crate) room_id: String,
    pub(crate) language: String,
    pub(crate) client_theme: String,
    pub(crate) client_id: String,
    pub(crate) device_id: String,
    pub(crate) homeserver_url: String,
}
pub fn replace_properties(url: &mut Url, props: QueryProperties) {
    let replace_map: [(&str, String); 10] = [
        (WIDGET_ID, encode(&props.widget_id).into()),
        (AVATAR_URL, encode(&props.avatar_url).into()),
        (DEVICE_ID, encode(&props.device_id).into()),
        (DISPLAY_NAME, encode(&props.display_name).into()),
        (HOMESERVER_URL, encode(&props.homeserver_url).into()),
        (USER_ID, encode(&props.user_id).into()),
        (ROOM_ID, encode(&props.room_id).into()),
        (LANGUAGE, encode(&props.language).into()),
        (CLIENT_THEME, encode(&props.client_theme).into()),
        (CLIENT_ID, encode(&props.client_id).into()),
    ]
    .map(|to_replace| {
        // Its save to unwrap here since we know all replace strings start with `$`
        (to_replace.0.get(1..).unwrap(), to_replace.1)
    });

    let s = url.as_str();
    let Some(beginning) = s.split_once('$').map(|s| s.0) else {
        // There is no '$' in the string so we don't need to do anything
        return;
    };
    let mut result = String::from(beginning);
    for section in s.split('$').skip(1) {
        let mut section_added = false;
        for (old, new) in &replace_map {
            section.split_once(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_'));
            // save to unwrap here since we know all replace strings start with $
            if section.starts_with(old) {
                result.push_str(new);
                if let Some(rest) = section.get(old.len()..) {
                    result.push_str(rest);
                }
                section_added = true;
            }
        }
        if !section_added {
            result.push_str(section);
        }
    }
    *url = Url::parse(&result).unwrap();
}
#[cfg(test)]

mod tests {
    use url::Url;

    use super::{replace_properties, QueryProperties};
    const EXAMPLE_URL: &str = "\
    https://my.widget.org/custom/path/using/$matrix_display_name/in/it\
    ?widgetId=$matrix_widget_id\
    &deviceId=$org.matrix.msc2873.matrix_device_id\
    &avatarUrl=$matrix_avatar_url\
    &displayname=$matrix_display_name\
    &lang=$org.matrix.msc2873.client_language\
    &theme=$org.matrix.msc2873.client_theme\
    &clientId=$org.matrix.msc2873.client_id\
    &baseUrl=$org.matrix.msc4039.matrix_base_url\
    #andAHashWithA$org.matrix.msc2873.client_themeThemeAndTheClientId:$org.matrix.msc2873.client_id\
    ";

    fn get_example_props() -> QueryProperties {
        QueryProperties {
            widget_id: String::from("!@/abc_widget_id"),
            avatar_url: "!@/abc_avatar_url".to_owned(),
            display_name: "I_AM_THEuser".to_owned(),
            user_id: "!@/abc_user_id".to_owned(),
            room_id: "!@/abc_room_id".to_owned(),
            language: "!@/abc_language".to_owned(),
            client_theme: "light".to_owned(),
            client_id: "12345678".to_owned(),
            device_id: "!@/abc_device_id".to_owned(),
            homeserver_url: "https://abc_base_url/".to_owned(),
        }
    }

    fn get_example_url() -> Url {
        Url::parse(EXAMPLE_URL).expect("EXAMPLE_URL is malformatted")
    }

    #[test]
    fn replace_all_properties() {
        let mut url = get_example_url();

        const CONVERTED_URL: &str = "https://my.widget.org/custom/path/using/I_AM_THEuser/in/it?widgetId=%21%40%2Fabc_widget_id&deviceId=%21%40%2Fabc_device_id&avatarUrl=%21%40%2Fabc_avatar_url&displayname=I_AM_THEuser&lang=%21%40%2Fabc_language&theme=light&clientId=12345678&baseUrl=https%3A%2F%2Fabc_base_url%2F#andAHashWithAlightThemeAndTheClientId:12345678";

        replace_properties(&mut url, get_example_props());
        assert_eq!(url.as_str(), CONVERTED_URL);
    }
}
