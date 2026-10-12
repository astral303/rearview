-- OpenCode 2's two session tables, transcribed from v2.0.26's
-- packages/core/src/database/schema.gen.ts, keeping the NOT NULL columns
-- without a default and the ones rearview's tests read. A database OpenCode 2
-- creates holds these and no `session`, `message` or `part` table; one it
-- upgrades holds them beside OpenCode 1's tables.
CREATE TABLE `session_v2` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `parent_id` text,
  `slug` text NOT NULL,
  `directory` text NOT NULL,
  `title` text,
  `version` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL
);
CREATE TABLE `session_message` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `type` text NOT NULL,
  `seq` integer NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL
);
