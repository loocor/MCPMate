CREATE TABLE IF NOT EXISTS workflow_profile_skill_settings (
    profile_id TEXT PRIMARY KEY,
    package_distribution TEXT NOT NULL CHECK (package_distribution IN ('symlink', 'copy')),
    FOREIGN KEY (profile_id) REFERENCES workflow_profile_skills (profile_id) ON DELETE CASCADE
);
