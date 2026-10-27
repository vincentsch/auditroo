//! Built-in recipes: ready-made custom checks that `auditroo recipes copy`
//! adds to a config file.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use crate::config::{validate_config_structure_for_copy, ConfigError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    pub id: &'static str,
    pub description: &'static str,
    pub page_type: &'static str,
    pub fragment: &'static str,
}

pub const RECIPES: &[Recipe] = &[
    Recipe {
        id: "checkout-and-pricing",
        description: "Checks that checkout and pricing pages can be indexed and have your analytics tag. Canonical tags and redirects must stay on your production host, and the pages must not mention a staging host.",
        page_type: "checkout/pricing",
        fragment: include_str!("../recipes/checkout-and-pricing.toml"),
    },
    Recipe {
        id: "documentation-site",
        description: "Checks that docs pages have a title and a meta description, and exactly one H1. The canonical tag must name your production host, and the JSON-LD needs a breadcrumb and an Article with a headline and a date.",
        page_type: "documentation",
        fragment: include_str!("../recipes/documentation-site.toml"),
    },
    Recipe {
        id: "product-pages",
        description: "Checks that product pages can be indexed and have one Product JSON-LD block with a price and an availability. The canonical tag must name your production host.",
        page_type: "product",
        fragment: include_str!("../recipes/product-pages.toml"),
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeError {
    UnknownRecipe(String),
    Config(ConfigError),
    Io(String),
}

impl std::fmt::Display for RecipeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRecipe(id) => write!(
                formatter,
                "there is no recipe {id:?}. auditroo recipes list shows the recipes"
            ),
            Self::Config(error) => write!(formatter, "{error}"),
            Self::Io(detail) => write!(formatter, "could not copy the recipe: {detail}"),
        }
    }
}

impl std::error::Error for RecipeError {}

impl From<ConfigError> for RecipeError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<io::Error> for RecipeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

pub fn find_recipe(id: &str) -> Result<Recipe, RecipeError> {
    RECIPES
        .iter()
        .copied()
        .find(|recipe| recipe.id == id)
        .ok_or_else(|| RecipeError::UnknownRecipe(id.to_owned()))
}

pub fn render_recipe_list() -> String {
    let mut output = String::new();
    for recipe in RECIPES {
        output.push_str(&format!(
            "{}\t{}\t{}\n",
            recipe.id, recipe.page_type, recipe.description
        ));
    }
    output
}

pub fn render_recipe(recipe: Recipe) -> String {
    recipe.fragment.to_owned()
}

pub fn copy_recipe(id: &str, destination: &Path) -> Result<(), RecipeError> {
    let recipe = find_recipe(id)?;
    let parent = crate::snapshot::parent_dir(destination);
    if !parent.exists() {
        return Err(RecipeError::Io(format!(
            "the folder {} does not exist",
            parent.display()
        )));
    }

    let existing = match fs::read(destination) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let merged = merge_recipe_bytes(&existing, recipe.fragment.as_bytes());
    validate_config_structure_for_copy(std::str::from_utf8(&merged).map_err(|error| {
        RecipeError::Io(format!("the config file is not UTF-8 text: {error}"))
    })?)?;
    write_atomically(destination, &merged)?;
    Ok(())
}

fn merge_recipe_bytes(existing: &[u8], fragment: &[u8]) -> Vec<u8> {
    if existing.is_empty() {
        return fragment.to_vec();
    }
    let mut merged = existing.to_vec();
    let trailing_lf = existing
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\n')
        .count();
    match trailing_lf {
        0 => merged.extend_from_slice(b"\n\n"),
        1 => merged.push(b'\n'),
        _ => {}
    }
    merged.extend_from_slice(fragment);
    merged
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), RecipeError> {
    let parent = crate::snapshot::parent_dir(path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auditroo.toml");
    let mut temp_path = None;
    let mut temp_file = None;
    for attempt in 0..1000u32 {
        let candidate = parent.join(format!(".{file_name}.tmp-{}-{attempt}", std::process::id()));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => {
                temp_path = Some(candidate);
                temp_file = Some(file);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }

    let temp_path = temp_path.ok_or_else(|| {
        RecipeError::Io("could not create a temporary file next to the config".to_owned())
    })?;
    let mut temp_file = temp_file.expect("temporary file was opened with its path");
    let result = (|| -> Result<(), RecipeError> {
        temp_file.write_all(bytes)?;
        temp_file.flush()?;
        temp_file.sync_all()?;
        drop(temp_file);
        fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipe_fragments_end_with_one_lf() {
        for recipe in RECIPES {
            assert!(recipe.fragment.ends_with('\n'));
            assert!(!recipe.fragment.ends_with("\n\n"));
        }
    }

    #[test]
    fn merging_preserves_existing_bytes_with_blank_line() {
        assert_eq!(merge_recipe_bytes(b"", b"fragment\n"), b"fragment\n");
        assert_eq!(merge_recipe_bytes(b"old", b"new\n"), b"old\n\nnew\n");
        assert_eq!(merge_recipe_bytes(b"old\n", b"new\n"), b"old\n\nnew\n");
        assert_eq!(merge_recipe_bytes(b"old\n\n", b"new\n"), b"old\n\nnew\n");
    }
}
