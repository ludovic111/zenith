//! Port of `apps/server/src/environmentTheme.test.ts`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use zc_settings::themes::EnvironmentThemeService;

fn nightfall() -> Value {
    json!({"name": "Nightfall", "appearance": "dark", "canvas": "#1a1b26", "accent": "#7aa2f7"})
}

/// The standard exported form: a full palette, no seeds.
fn shared_light() -> Value {
    json!({"version": 1, "name": "Shared Light", "appearance": "light", "colors": {"canvas": "#eff1f5", "accent": "#1e66f5"}})
}

fn with_id(id: &str, file: Value) -> Value {
    let mut theme = json!({"id": id});
    theme.as_object_mut().unwrap().extend(file.as_object().unwrap().clone());
    theme
}

struct Fixture {
    dir: tempfile::TempDir,
    themes_dir: PathBuf,
}

impl Fixture {
    /// Seeds theme files before the service starts, as a real machine would.
    fn new(seeds: &[(&str, String)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let themes_dir = dir.path().join("userdata").join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();
        for (name, contents) in seeds {
            std::fs::write(themes_dir.join(name), contents).unwrap();
        }
        Self { dir, themes_dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.themes_dir.join(name)
    }
}

fn ids(themes: &[Value]) -> Vec<&str> {
    themes.iter().map(|theme| theme["id"].as_str().unwrap()).collect()
}

fn encode(file: Value) -> String {
    file.to_string()
}

#[tokio::test]
async fn publishes_nothing_when_the_machine_has_no_theme_files() {
    let fixture = Fixture::new(&[]);
    let service = EnvironmentThemeService::start(&fixture.themes_dir).await;
    assert_eq!(service.current().await, Vec::<Value>::new());
}

#[tokio::test]
async fn publishes_each_file_under_its_filename_as_the_id() {
    let fixture = Fixture::new(&[("nightfall.json", encode(nightfall())), ("shared-light.json", encode(shared_light()))]);
    let themes = EnvironmentThemeService::start(&fixture.themes_dir).await.current().await;
    assert_eq!(ids(&themes), ["nightfall", "shared-light"]);
    assert_eq!(themes[0], with_id("nightfall", nightfall()));
    assert_eq!(themes[1], with_id("shared-light", shared_light()));
}

#[tokio::test]
async fn follows_the_directory_rather_than_the_set_read_at_start() {
    let fixture = Fixture::new(&[("nightfall.json", encode(nightfall()))]);
    let service = EnvironmentThemeService::start(&fixture.themes_dir).await;
    std::fs::write(fixture.path("shared-light.json"), encode(shared_light())).unwrap();
    assert_eq!(service.current().await.len(), 2);
    std::fs::remove_file(fixture.path("nightfall.json")).unwrap();
    assert_eq!(ids(&service.current().await), ["shared-light"]);
}

#[tokio::test]
async fn streams_the_current_set_first() {
    let fixture = Fixture::new(&[("nightfall.json", encode(nightfall()))]);
    let service = EnvironmentThemeService::start(&fixture.themes_dir).await;
    let first = service.stream_changes().await.next().await.unwrap();
    assert_eq!(first, vec![with_id("nightfall", nightfall())]);
}

#[tokio::test]
async fn never_replays_a_set_older_than_the_snapshot_it_started_from() {
    let fixture = Fixture::new(&[("nightfall.json", encode(nightfall()))]);
    let service = EnvironmentThemeService::start(&fixture.themes_dir).await;
    std::fs::write(fixture.path("shared-light.json"), encode(shared_light())).unwrap();
    let mut stream = service.stream_changes().await;
    let first = stream.next().await.unwrap();
    assert_eq!(ids(&first), ["nightfall", "shared-light"]);
    // Nothing older follows the snapshot.
    let next = tokio::time::timeout(Duration::from_millis(300), stream.next()).await;
    if let Ok(Some(update)) = next {
        assert_eq!(ids(&update), ["nightfall", "shared-light"]);
    }
}

#[tokio::test]
async fn skips_invalid_files_while_keeping_valid_ones() {
    let fixture = Fixture::new(&[
        ("nightfall.json", encode(nightfall())),
        (
            "unresolved.json",
            r##"{ "name": "X", "appearance": "dark", "canvas": "{{ background }}", "accent": "#7aa2f7" }"##.to_owned(),
        ),
        ("malformed.json", "{ not json".to_owned()),
        ("no-colors.json", r#"{ "name": "Empty", "appearance": "dark" }"#.to_owned()),
        ("Bad Name.json", encode(shared_light())),
        ("ocean.json", encode(shared_light())),
        ("dark.json", encode(shared_light())),
        ("notes.txt", "not a theme".to_owned()),
    ]);
    let themes = EnvironmentThemeService::start(&fixture.themes_dir).await.current().await;
    assert_eq!(ids(&themes), ["nightfall"]);
}

#[cfg(unix)]
#[tokio::test]
async fn ignores_a_symlinked_theme_file() {
    let fixture = Fixture::new(&[]);
    let outside = fixture.themes_dir.parent().unwrap().join("outside.json");
    std::fs::write(&outside, encode(nightfall())).unwrap();
    std::os::unix::fs::symlink(&outside, fixture.path("nightfall.json")).unwrap();
    let themes = EnvironmentThemeService::start(&fixture.themes_dir).await.current().await;
    assert_eq!(themes, Vec::<Value>::new());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_themes_directory_stays_usable() {
    let fixture = Fixture::new(&[("nightfall.json", encode(nightfall()))]);
    let link = fixture.dir.path().join("themes-link");
    std::os::unix::fs::symlink(&fixture.themes_dir, &link).unwrap();
    let themes = EnvironmentThemeService::start(&link).await.current().await;
    assert_eq!(ids(&themes), ["nightfall"]);
}

#[tokio::test]
async fn does_not_charge_skipped_files_against_the_total_size_limit() {
    let mut seeds: Vec<(String, String)> = (0..7).map(|index| (format!("junk-{index}.json"), "{".repeat(30_000))).collect();
    seeds.push(("zz-valid.json".into(), encode(nightfall())));
    let seeds: Vec<(&str, String)> = seeds.iter().map(|(name, contents)| (name.as_str(), contents.clone())).collect();
    let fixture = Fixture::new(&seeds);
    let themes = EnvironmentThemeService::start(&fixture.themes_dir).await.current().await;
    assert_eq!(ids(&themes), ["zz-valid"]);
}

#[tokio::test]
async fn caps_the_files_examined_and_their_size() {
    let mut seeds: Vec<(String, String)> = (0..33).map(|index| (format!("t{index:02}.json"), encode(nightfall()))).collect();
    seeds.push(("big.json".into(), format!("{}{}", " ".repeat(33 * 1024), encode(nightfall()))));
    let seeds: Vec<(&str, String)> = seeds.iter().map(|(name, contents)| (name.as_str(), contents.clone())).collect();
    let fixture = Fixture::new(&seeds);
    let themes = EnvironmentThemeService::start(&fixture.themes_dir).await.current().await;
    // `big.json` sorts first and is too large; then 32 files are examined at most.
    assert_eq!(themes.len(), 31);
    assert_eq!(themes[0]["id"], json!("t00"));
}

async fn next_set(stream: &mut futures::stream::BoxStream<'static, Vec<Value>>) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("a set within 10 s")
        .expect("the stream stays open")
}

#[tokio::test]
async fn streams_a_set_for_every_change_to_the_directory() {
    let fixture = Fixture::new(&[]);
    let service = EnvironmentThemeService::start(&fixture.themes_dir).await;
    let mut stream = service.stream_changes().await;
    assert_eq!(next_set(&mut stream).await, Vec::<Value>::new());
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Published atomically, the way a theme hook writes it.
    let staging: &Path = &fixture.dir.path().join("staged.json");
    std::fs::write(staging, encode(nightfall())).unwrap();
    std::fs::rename(staging, fixture.path("nightfall.json")).unwrap();
    assert_eq!(ids(&next_set(&mut stream).await), ["nightfall"]);
    std::fs::remove_file(fixture.path("nightfall.json")).unwrap();
    assert_eq!(next_set(&mut stream).await, Vec::<Value>::new());
}
