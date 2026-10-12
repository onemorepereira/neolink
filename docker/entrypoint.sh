#!/bin/sh
# Kept for setups that name the old entrypoint. neolink itself is the entrypoint now: it
# runs its own arguments, and execs any other command (`sh -c "..."`) as this script did.
exec /usr/local/bin/neolink "$@"
