-- Counting recent trials reads this on every new create_project.
CREATE INDEX runlater_project_created_at ON runlater_project (created_at);
