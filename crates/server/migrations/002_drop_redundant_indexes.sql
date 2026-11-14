-- Drop the two indexes that duplicate UNIQUE constraints (001_init.sql
-- used to create them; the UNIQUE already builds an implicit index, so
-- these only cost space and write time). Runs on every migrate, so
-- databases created from the old 001 pick the drop up too.
DROP INDEX IF EXISTS idx_cache_hash;
DROP INDEX IF EXISTS idx_chapters_book;
