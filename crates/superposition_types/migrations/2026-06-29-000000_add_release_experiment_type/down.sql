-- This file should undo anything in `up.sql`
CREATE TYPE public.experiment_type_new as enum ('DEFAULT', 'DELETE_OVERRIDES');
ALTER TABLE public.experiments ALTER COLUMN experiment_type DROP DEFAULT;
ALTER TABLE public.experiments ALTER COLUMN experiment_type TYPE public.experiment_type_new USING experiment_type::text::public.experiment_type_new;
ALTER TABLE public.experiments ALTER COLUMN experiment_type SET DEFAULT 'DEFAULT';
DROP TYPE public.experiment_type;
ALTER TYPE public.experiment_type_new RENAME TO experiment_type;
