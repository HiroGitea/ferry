-- Minimum memory per GPU a job/task asks for (0 = any card).
ALTER TABLE jobs ADD COLUMN gpu_mem_bytes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN gpu_mem_bytes INTEGER NOT NULL DEFAULT 0;
