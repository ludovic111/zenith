//! External agent sessions: discovery (`AgentSessionScanner.ts`), import
//! (`AgentSessionImporter.ts`) and the streaming transcript reader (`AgentSessionJson.ts`).

pub mod fs;
pub mod importer;
pub mod json;
pub mod scanner;
pub mod transcript;

pub use importer::{
    AgentSessionImporter, EngineImport, ImportBinding, ImportEngine, ImportReads, ImportSessionDirectory, ProjectionImportReads, RecentThreadsSource,
};
pub use scanner::{AgentSessionScanner, RecentThread, ScannerConfig, SettingsSource, StaticSettings};
pub use transcript::{parse_agent_session_transcript, AgentSessionThread, ThreadMessage, TranscriptMetadata};
