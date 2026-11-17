//! English plurals and singulars, as both frameworks guess table names from
//! column names (`author_id` → `authors`) and back. Their inflectors also
//! know irregular words (`person` → `people`); these are the regular rules.

/// `category` → `categories`, `box` → `boxes`, `user` → `users`.
pub(super) fn plural(word: &str) -> String {
    let consonant_y = word.len() > 1 && word.ends_with('y') && !word[..word.len() - 1].ends_with(['a', 'e', 'i', 'o', 'u']);
    if consonant_y {
        format!("{}ies", &word[..word.len() - 1])
    } else if ["s", "x", "z", "ch", "sh"].iter().any(|e| word.ends_with(e)) {
        format!("{word}es")
    } else {
        format!("{word}s")
    }
}

/// `categories` → `category`, `boxes` → `box`, `users` → `user`.
pub(super) fn singular(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies") {
        format!("{stem}y")
    } else if let Some(stem) = word.strip_suffix("es")
        && ["s", "x", "z", "ch", "sh"].iter().any(|e| stem.ends_with(e))
    {
        stem.to_string()
    } else {
        word.strip_suffix('s').unwrap_or(word).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_regular_rules() {
        for (one, many) in [("category", "categories"), ("day", "days"), ("box", "boxes"), ("branch", "branches"), ("user", "users")] {
            assert_eq!(plural(one), many);
            assert_eq!(singular(many), one);
        }
    }
}
