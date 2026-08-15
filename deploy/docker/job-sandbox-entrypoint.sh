#!/bin/sh
# The job container's pid 1 (MAIN-611).
#
# It sets the container up and then does nothing: the AGENT is `docker exec`ed
# in by the node, after the node has installed the egress policy. That ordering
# is the point — nothing untrusted runs until the firewall is up, and this
# script is the only thing that runs before it.
set -eu

AGENT_UID="${NOOK_SANDBOX_UID:-1000}"
AGENT_GID="${NOOK_SANDBOX_GID:-1000}"

# The agent runs as the NODE's uid, so what it writes into the bind-mounted
# checkout is owned by the node's user and the prune that follows can delete it
# (MAIN-537). The container itself stays root: the nested daemon and the
# firewall both need that, and neither is the agent.
if ! getent group "$AGENT_GID" >/dev/null 2>&1; then
  groupadd -g "$AGENT_GID" agent
fi
if ! getent passwd "$AGENT_UID" >/dev/null 2>&1; then
  useradd -u "$AGENT_UID" -g "$AGENT_GID" -d /home/agent -M -s /bin/bash agent
fi
mkdir -p /home/agent
chown "$AGENT_UID:$AGENT_GID" /home/agent

# Only a kind whose profile declares it gets a daemon (AC-12). A spec or review
# agent gets none at all — it cannot run containers, and it does not pay a cold
# start to file a ticket.
#
# The daemon is root's and the agent is not, so `--group` is what lets the
# agent's uid reach the socket without being root itself. It is a NESTED daemon
# with its own storage: `docker run -v /:/host` inside the job mounts THIS
# container's root — a scratch filesystem — and never the machine's (AC-3, AC-4).
if [ "${NOOK_SANDBOX_DOCKER:-0}" = "1" ]; then
  # Docker mounts /sys/fs/cgroup read-only in a container that is not
  # `--privileged`, and a daemon inside cannot then create a cgroup for the
  # containers it starts ("mkdir /sys/fs/cgroup/docker: read-only file
  # system"). The remount is over this container's OWN cgroup namespace
  # (`--cgroupns=private`), so it reaches nothing of the host's tree — unlike
  # the `-v /sys/fs/cgroup:/sys/fs/cgroup:rw` every DinD recipe reaches for.
  mount -o remount,rw /sys/fs/cgroup 2>/dev/null || \
    echo "warning: could not make /sys/fs/cgroup writable — nested containers will not start" >&2
  # …and the same for the network sysctls the daemon sets on every veth it
  # creates ("failed to disable IPv6 on container's interface eth0").
  #
  # /proc/sys/net ALONE, re-bound as its own mount — never `remount,rw
  # /proc/sys`, which would also unlock /proc/sys/kernel and let a job set
  # sysctls on the HOST: those are not namespaced, and this container holds
  # CAP_SYS_ADMIN in the initial user namespace. Verified on 2026-08-15 that
  # after this, /proc/sys/net is writable and /proc/sys/kernel is not.
  mount --bind /proc/sys/net /proc/sys/net 2>/dev/null && \
    mount -o remount,rw,bind /proc/sys/net 2>/dev/null || \
    echo "warning: /proc/sys/net is read-only — nested container networking will fail" >&2
  dockerd --group "$AGENT_GID" --host=unix:///var/run/docker.sock \
    >/var/log/dockerd.log 2>&1 &
fi

# Nothing else to do. The node exec's the agent in, and removing this container
# is what ends the run — every process inside goes with it, nested daemon and
# compose stack included.
exec tail -f /dev/null
