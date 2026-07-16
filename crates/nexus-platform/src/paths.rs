use std::array;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// A key in the legacy Nexus path index.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(usize)]
pub enum PathKey {
    /// The operating-system directory containing `d3d11.dll`.
    SystemDirectory,
    /// The Guild Wars 2 installation directory.
    GameDirectory,
    /// `<game>/addons`.
    AddonsDirectory,
    /// `<game>/addons/common`.
    CommonDirectory,
    /// The Guild Wars 2 API cache directory.
    GuildWars2ApiCacheDirectory,
    /// The Raidcore API cache directory.
    RaidcoreApiCacheDirectory,
    /// The GitHub API cache directory.
    GitHubApiCacheDirectory,
    /// `<game>/addons/Nexus`.
    NexusDirectory,
    /// The Nexus temporary directory.
    TempDirectory,
    /// The Nexus font directory.
    FontsDirectory,
    /// The Nexus locale directory.
    LocalesDirectory,
    /// The Nexus style directory.
    StylesDirectory,
    /// The Nexus texture directory.
    TexturesDirectory,
    /// The current user's documents directory.
    DocumentsDirectory,
    /// `<documents>/Guild Wars 2`.
    GuildWars2DocumentsDirectory,
    /// The Guild Wars 2 input-bind directory.
    GuildWars2InputBindsDirectory,
    /// The loaded Nexus proxy module.
    NexusDll,
    /// The update candidate beside the proxy module.
    NexusDllUpdate,
    /// The previous proxy module beside the proxy module.
    NexusDllOld,
    /// The system `d3d11.dll`.
    SystemD3d11,
    /// The optional game-directory chainload DLL.
    D3d11Chainload,
    /// The default Nexus log.
    Log,
    /// The crash log.
    CrashLog,
    /// The crash stack log.
    CrashStack,
    /// The Nexus input-bind JSON file.
    InputBinds,
    /// The game-bind XML file.
    GameBinds,
    /// The Nexus settings JSON file.
    Settings,
    /// The default addon configuration JSON file.
    AddonConfigDefault,
    /// The arcdps integration DLL.
    ArcdpsIntegration,
    /// The bundled third-party software notice.
    ThirdPartySoftwareReadme,
    /// The English locale file.
    LocaleEnglish,
    /// The German locale file.
    LocaleGerman,
    /// The French locale file.
    LocaleFrench,
    /// The Spanish locale file.
    LocaleSpanish,
    /// The Chinese locale file.
    LocaleChinese,
    /// The Korean locale file.
    LocaleKorean,
    /// The Brazilian Portuguese locale file.
    LocaleBrazilianPortuguese,
    /// The Czech locale file.
    LocaleCzech,
    /// The Italian locale file.
    LocaleItalian,
    /// The Polish locale file.
    LocalePolish,
    /// The Russian locale file.
    LocaleRussian,
}

impl PathKey {
    const COUNT: usize = 41;

    /// Every key, in the same order as the legacy `EPath` enumeration.
    pub const ALL: [Self; Self::COUNT] = [
        Self::SystemDirectory,
        Self::GameDirectory,
        Self::AddonsDirectory,
        Self::CommonDirectory,
        Self::GuildWars2ApiCacheDirectory,
        Self::RaidcoreApiCacheDirectory,
        Self::GitHubApiCacheDirectory,
        Self::NexusDirectory,
        Self::TempDirectory,
        Self::FontsDirectory,
        Self::LocalesDirectory,
        Self::StylesDirectory,
        Self::TexturesDirectory,
        Self::DocumentsDirectory,
        Self::GuildWars2DocumentsDirectory,
        Self::GuildWars2InputBindsDirectory,
        Self::NexusDll,
        Self::NexusDllUpdate,
        Self::NexusDllOld,
        Self::SystemD3d11,
        Self::D3d11Chainload,
        Self::Log,
        Self::CrashLog,
        Self::CrashStack,
        Self::InputBinds,
        Self::GameBinds,
        Self::Settings,
        Self::AddonConfigDefault,
        Self::ArcdpsIntegration,
        Self::ThirdPartySoftwareReadme,
        Self::LocaleEnglish,
        Self::LocaleGerman,
        Self::LocaleFrench,
        Self::LocaleSpanish,
        Self::LocaleChinese,
        Self::LocaleKorean,
        Self::LocaleBrazilianPortuguese,
        Self::LocaleCzech,
        Self::LocaleItalian,
        Self::LocalePolish,
        Self::LocaleRussian,
    ];
}

/// Injectable roots used to construct the compatibility path tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathRoots {
    module_path: PathBuf,
    system_directory: PathBuf,
    documents_directory: PathBuf,
}

impl PathRoots {
    /// Creates roots without reading or mutating the host environment.
    pub fn new(
        module_path: impl Into<PathBuf>,
        system_directory: impl Into<PathBuf>,
        documents_directory: impl Into<PathBuf>,
    ) -> Self {
        Self {
            module_path: module_path.into(),
            system_directory: system_directory.into(),
            documents_directory: documents_directory.into(),
        }
    }
}

/// A fully prepared, Unicode-safe compatibility path index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathIndex {
    paths: [PathBuf; PathKey::COUNT],
}

impl PathIndex {
    /// Prepares the exact legacy tree without creating directories.
    ///
    /// # Errors
    ///
    /// Returns [`PathError::ModuleHasNoParent`] when the module path cannot
    /// identify a game directory.
    pub fn prepare(roots: PathRoots) -> Result<Self, PathError> {
        let game_directory = roots
            .module_path
            .parent()
            .ok_or(PathError::ModuleHasNoParent)?
            .to_path_buf();
        let mut paths = array::from_fn(|_| PathBuf::new());

        set(&mut paths, PathKey::SystemDirectory, roots.system_directory);
        set(&mut paths, PathKey::GameDirectory, game_directory.clone());
        set(
            &mut paths,
            PathKey::AddonsDirectory,
            game_directory.join("addons"),
        );
        let addons = paths[PathKey::AddonsDirectory as usize].clone();
        set(&mut paths, PathKey::CommonDirectory, addons.join("common"));
        let common = paths[PathKey::CommonDirectory as usize].clone();
        set(
            &mut paths,
            PathKey::GuildWars2ApiCacheDirectory,
            common.join("api.guildwars2.com"),
        );
        set(
            &mut paths,
            PathKey::RaidcoreApiCacheDirectory,
            common.join("api.raidcore.gg"),
        );
        set(
            &mut paths,
            PathKey::GitHubApiCacheDirectory,
            common.join("api.github.com"),
        );
        set(&mut paths, PathKey::NexusDirectory, addons.join("Nexus"));
        let nexus = paths[PathKey::NexusDirectory as usize].clone();
        set(&mut paths, PathKey::TempDirectory, nexus.join("Temp"));
        set(&mut paths, PathKey::FontsDirectory, nexus.join("Fonts"));
        set(&mut paths, PathKey::LocalesDirectory, nexus.join("Locales"));
        set(&mut paths, PathKey::StylesDirectory, nexus.join("Styles"));
        set(
            &mut paths,
            PathKey::TexturesDirectory,
            nexus.join("Textures"),
        );

        set(
            &mut paths,
            PathKey::DocumentsDirectory,
            roots.documents_directory,
        );
        let documents = paths[PathKey::DocumentsDirectory as usize].clone();
        set(
            &mut paths,
            PathKey::GuildWars2DocumentsDirectory,
            documents.join("Guild Wars 2"),
        );
        let gw2_documents = paths[PathKey::GuildWars2DocumentsDirectory as usize].clone();
        set(
            &mut paths,
            PathKey::GuildWars2InputBindsDirectory,
            gw2_documents.join("InputBinds"),
        );

        set(&mut paths, PathKey::NexusDll, roots.module_path.clone());
        set(
            &mut paths,
            PathKey::NexusDllUpdate,
            append_suffix(&roots.module_path, ".update"),
        );
        set(
            &mut paths,
            PathKey::NexusDllOld,
            append_suffix(&roots.module_path, ".old"),
        );
        let system = paths[PathKey::SystemDirectory as usize].clone();
        set(&mut paths, PathKey::SystemD3d11, system.join("d3d11.dll"));
        set(
            &mut paths,
            PathKey::D3d11Chainload,
            game_directory.join("d3d11_chainload.dll"),
        );

        for (key, filename) in [
            (PathKey::Log, "Nexus.log"),
            (PathKey::CrashLog, "Crash.log"),
            (PathKey::CrashStack, "CrashStack.log"),
            (PathKey::InputBinds, "InputBinds.json"),
            (PathKey::GameBinds, "GameBinds.xml"),
            (PathKey::Settings, "Settings.json"),
            (PathKey::AddonConfigDefault, "AddonConfig.json"),
            (PathKey::ArcdpsIntegration, "arcdps_integration64.dll"),
            (
                PathKey::ThirdPartySoftwareReadme,
                "THIRDPARTYSOFTWAREREADME.TXT",
            ),
        ] {
            set(&mut paths, key, nexus.join(filename));
        }

        let locales = paths[PathKey::LocalesDirectory as usize].clone();
        for (key, filename) in [
            (PathKey::LocaleEnglish, "en_Main.json"),
            (PathKey::LocaleGerman, "de_Main.json"),
            (PathKey::LocaleFrench, "fr_Main.json"),
            (PathKey::LocaleSpanish, "es_Main.json"),
            (PathKey::LocaleChinese, "cn_Main.json"),
            (PathKey::LocaleKorean, "kr_Main.json"),
            (PathKey::LocaleBrazilianPortuguese, "br_Main.json"),
            (PathKey::LocaleCzech, "cz_Main.json"),
            (PathKey::LocaleItalian, "it_Main.json"),
            (PathKey::LocalePolish, "pl_Main.json"),
            (PathKey::LocaleRussian, "ru_Main.json"),
        ] {
            set(&mut paths, key, locales.join(filename));
        }

        debug_assert!(paths.iter().all(|path| !path.as_os_str().is_empty()));
        Ok(Self { paths })
    }

    /// Returns an indexed path.
    #[must_use]
    pub fn get(&self, key: PathKey) -> &Path {
        &self.paths[key as usize]
    }

    /// Joins an addon name using the legacy addon-directory semantics.
    #[must_use]
    pub fn addon_directory(&self, name: impl AsRef<OsStr>) -> PathBuf {
        let name = name.as_ref();
        if name.is_empty() {
            self.get(PathKey::AddonsDirectory).to_path_buf()
        } else {
            self.get(PathKey::AddonsDirectory).join(name)
        }
    }

    /// Creates only the directories created by legacy `CreateIndex`.
    ///
    /// The preparation step itself is side-effect free, making startup intent
    /// explicit and keeping tests away from real user directories.
    ///
    /// # Errors
    ///
    /// Returns a redacted error identifying only the fixed path key whose
    /// directory could not be created.
    pub fn create_directories(&self) -> Result<(), PathError> {
        for key in [
            PathKey::AddonsDirectory,
            PathKey::CommonDirectory,
            PathKey::NexusDirectory,
            PathKey::TempDirectory,
            PathKey::FontsDirectory,
            PathKey::LocalesDirectory,
            PathKey::StylesDirectory,
            PathKey::TexturesDirectory,
        ] {
            std::fs::create_dir_all(self.get(key))
                .map_err(|source| PathError::CreateDirectory { key, source })?;
        }
        Ok(())
    }
}

/// A closed, path-redacted path-index error.
#[derive(Debug, Error)]
pub enum PathError {
    /// The module path did not have a parent directory.
    #[error("the module path has no parent directory")]
    ModuleHasNoParent,
    /// A fixed compatibility directory could not be created.
    #[error("could not create compatibility directory {key:?}")]
    CreateDirectory {
        /// The fixed, non-sensitive path key.
        key: PathKey,
        /// The operating-system error, which does not contain the requested path.
        #[source]
        source: std::io::Error,
    },
}

fn set(paths: &mut [PathBuf; PathKey::COUNT], key: PathKey, value: PathBuf) {
    paths[key as usize] = value;
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nexus-platform-paths-{}-{id}-Živjo_日本",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("test root should be created");
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prepares_exact_legacy_names_without_mutation() {
        let temp = TempRoot::new();
        let game = temp.0.join("Igra_日本");
        let module = game.join("d3d11.dll");
        let system = temp.0.join("Sistem");
        let documents = temp.0.join("Dokumenti");
        let index = PathIndex::prepare(PathRoots::new(&module, &system, &documents))
            .expect("valid roots should prepare");

        assert_eq!(index.get(PathKey::GameDirectory), game);
        assert_eq!(
            index.get(PathKey::Settings),
            game.join("addons/Nexus/Settings.json")
        );
        assert_eq!(
            index.get(PathKey::ThirdPartySoftwareReadme),
            game.join("addons/Nexus/THIRDPARTYSOFTWAREREADME.TXT")
        );
        assert_eq!(
            index.get(PathKey::NexusDllUpdate),
            PathBuf::from(format!("{}.update", module.display()))
        );
        assert!(!game.join("addons").exists());
        assert_eq!(PathKey::ALL.len(), 41);
    }

    #[test]
    fn creates_only_the_legacy_directory_tree_under_injected_root() {
        let temp = TempRoot::new();
        let game = temp.0.join("game");
        let index = PathIndex::prepare(PathRoots::new(
            game.join("d3d11.dll"),
            temp.0.join("system"),
            temp.0.join("documents"),
        ))
        .expect("valid roots should prepare");

        index
            .create_directories()
            .expect("injected tree should be created");

        for key in [
            PathKey::AddonsDirectory,
            PathKey::CommonDirectory,
            PathKey::NexusDirectory,
            PathKey::TempDirectory,
            PathKey::FontsDirectory,
            PathKey::LocalesDirectory,
            PathKey::StylesDirectory,
            PathKey::TexturesDirectory,
        ] {
            assert!(index.get(key).is_dir(), "missing {key:?}");
        }
        assert!(!index.get(PathKey::GuildWars2InputBindsDirectory).exists());
        assert!(!index.get(PathKey::GuildWars2ApiCacheDirectory).exists());
    }

    #[test]
    fn addon_directory_matches_legacy_empty_and_named_behavior() {
        let index = PathIndex::prepare(PathRoots::new(
            PathBuf::from("C:/game/d3d11.dll"),
            PathBuf::from("C:/system"),
            PathBuf::from("C:/documents"),
        ))
        .expect("valid roots should prepare");

        assert_eq!(
            index.addon_directory(""),
            index.get(PathKey::AddonsDirectory)
        );
        assert_eq!(
            index.addon_directory("ユニコード"),
            index.get(PathKey::AddonsDirectory).join("ユニコード")
        );
    }
}
