CREATE TABLE validators(
   epoch BIGINT,
   keybase_id TEXT,
   name TEXT,
   identity TEXT,
   vote_address TEXT,
   score BIGINT,
   avg_position DOUBLE precision,
   commission SMALLINT ,
   active_stake DOUBLE precision,
   epoch_credits BIGINT,
   data_center_concentration DOUBLE precision,
   can_halt_the_network_group BOOL,
   stake_state TEXT,
   stake_state_reason TEXT,
   www_url TEXT
);