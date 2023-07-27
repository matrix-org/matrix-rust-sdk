mod fuzzy_match_room_name;

pub use fuzzy_match_room_name::new_filter as new_filter_fuzzy_match_room_name;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

fn normalize_string(str: &str) -> String {
    str.nfd().filter(|c| !is_combining_mark(*c)).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_string() {
        assert_eq!(&normalize_string("abc"), "abc");
        assert_eq!(&normalize_string("Ștefan Été"), "Stefan Ete");
        assert_eq!(&normalize_string("Ç ṩ ḋ Å"), "C s d A");
        assert_eq!(&normalize_string("هند"), "هند");
    }
}
