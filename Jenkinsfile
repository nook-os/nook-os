// CI + deploy for NookOS.
//
// Build once, push to a registry, deploy by pulling. The deploy host never
// compiles anything and never needs the source tree to build from — a deploy
// is `docker compose pull && docker compose up -d`, which is fast, identical
// on every host, and rolls back by moving a tag.
//
// Deliberately knows nothing about where it runs. Every environment-specific
// value arrives as an environment variable set by the Jenkins job, so nothing
// about any particular network is committed here. See docs/ci-deploy.md.
//
//   NOOK_REGISTRY      image prefix to push to, e.g. registry.example.com/nookos
//   NOOK_REGISTRY_CRED Jenkins username/password credential id for it
//   NOOK_DEPLOY_DIR    checkout on the deploy host that compose runs from
//   NOOK_COMPOSE_FILE  compose file within it (default docker-compose.prod.yml)
//   NOOK_PG_CONTAINER  postgres container name, for the schema step
//   NOOK_HEALTH_URL    URL to poll after deploying
//   NOOK_DEPLOY_BRANCH branch allowed to deploy (default main)
//   NOOK_DEPLOY_IMAGE  run the deploy inside this image instead of on the
//                      agent — the way to deploy when the agent user can't
//                      read the deploy directory, since the container can run
//                      as root while Jenkins does not
//   NOOK_DEPLOY_MOUNTS extra `-v` arguments for that container
//   NOOK_DEPLOY_PREP   prerequisite install for that image, e.g.
//                      `apk add --no-cache bash coreutils curl`
//
// With no registry set this is a plain build-and-test pipeline, which is what
// a feature branch wants. The agent needs docker and git; toolchains come from
// containers, so there is nothing to install and nothing to drift.

def RUST_IMAGE = 'rust:1-slim-bookworm'
def NODE_IMAGE = 'node:22-slim'

// service -> dockerfile. The image name is the service name.
//
// No node image: a node's whole job is running the user's dev tooling in the
// user's checkouts, so it is joined natively on each machine rather than
// deployed as a container. Building it here would also mean compiling the
// workspace twice for something nothing pulls.
def IMAGES = [
    'nook-control': 'deploy/docker/control.Dockerfile',
    'nook-web'    : 'deploy/docker/web-prod.Dockerfile',
]

/**
 * Run a command in a throwaway container over the workspace.
 *
 * When Jenkins is itself a container, `-v $WORKSPACE:/w` silently does the
 * wrong thing: the daemon resolves bind sources on the host, where that path
 * doesn't exist, so the container gets an empty directory and the build fails
 * claiming the repo has no Cargo.toml. Reusing Jenkins' own mounts with
 * --volumes-from makes $WORKSPACE mean the same thing on both sides.
 *
 * `bash -c`, never `bash -lc`: a login shell re-reads /etc/profile, which
 * resets PATH and hides the toolchain the image put there — cargo vanishes
 * from an image built around cargo.
 *
 * The named volumes are what keep this from rebuilding the world every push.
 */
def inImage(String image, String extraArgs, String script) {
    sh """
        set -u
        if [ -f /.dockerenv ]; then
            WORKDIR="\$WORKSPACE"
            SHARE="--volumes-from \$(cat /etc/hostname)"
        else
            WORKDIR=/w
            SHARE="-v \$WORKSPACE:/w"
        fi
        STATUS=0
        docker run --rm \$SHARE \
          -v nook-ci-cargo:/usr/local/cargo/registry \
          -v nook-ci-target:"\$WORKDIR/target" \
          -w "\$WORKDIR" \
          ${extraArgs} ${image} \
          bash -c '${script}' || STATUS=\$?
        # These containers run as root and leave root-owned files (node_modules,
        # build output) in the workspace. Jenkins runs unprivileged, so without
        # this it eventually cannot clean — or even delete — its own job. Runs
        # on failure too, which is exactly when it is easiest to forget.
        docker run --rm \$SHARE -w "\$WORKDIR" alpine \
          chown -R "\$(id -u):\$(id -g)" . >/dev/null 2>&1 || true
        exit \$STATUS
    """
}


/**
 * Run a deploy command on the deploy host.
 *
 * Directly on the agent when it can reach the deploy directory itself, and
 * otherwise in a container: bind mounts are resolved by the daemon, which is
 * root, so a root container reads a root-owned directory that the Jenkins
 * user cannot. Mounted at its real path so compose sees the paths it expects.
 */
def onDeployHost(String script) {
    if ((env.NOOK_DEPLOY_IMAGE ?: '') == '') {
        sh "set -eu; cd \"\$NOOK_DEPLOY_DIR\"; ${script}"
        return
    }
    sh """
        set -eu
        docker run --rm -u 0 \
          -v /var/run/docker.sock:/var/run/docker.sock \
          -v "\$NOOK_DEPLOY_DIR":"\$NOOK_DEPLOY_DIR" \
          \${NOOK_DEPLOY_MOUNTS:-} \
          -w "\$NOOK_DEPLOY_DIR" \
          -e COMPOSE_FILE="\$COMPOSE_FILE" \
          -e DEPLOY_BRANCH="\$DEPLOY_BRANCH" \
          -e NOOK_PG_CONTAINER="\${NOOK_PG_CONTAINER:-}" \
          -e NOOK_DEPLOY_PREP="\${NOOK_DEPLOY_PREP:-true}" \
          "\$NOOK_DEPLOY_IMAGE" \
          sh -c 'eval "\$NOOK_DEPLOY_PREP" >/dev/null 2>&1 || true; set -eu; ${script}'
    """
}

/**
 * Is this build on the branch that is allowed to deploy?
 *
 * Multibranch jobs set BRANCH_NAME; a plain pipeline job doesn't, and the git
 * plugin's GIT_BRANCH ("origin/main") is what's available instead. Checking
 * only BRANCH_NAME would leave it null on a plain job and wave everything
 * through — so an unrecognised branch deploys nothing.
 */
def onDeployBranch() {
    def actual = env.BRANCH_NAME ?: env.GIT_BRANCH
    if (!actual) {
        echo 'No branch information available — refusing to deploy.'
        return false
    }
    return actual.replaceFirst(/^origin\//, '') == env.DEPLOY_BRANCH
}

pipeline {
    agent any

    options {
        // Only options core pipeline provides — no timestamps(), which would
        // make the timestamper plugin a hard requirement for anyone running
        // this.
        //
        // Deploys are not safe to interleave: two builds racing compose would
        // fight over the same containers.
        disableConcurrentBuilds()
        timeout(time: 60, unit: 'MINUTES')
        buildDiscarder(logRotator(numToKeepStr: '30'))
    }

    environment {
        COMPOSE_FILE  = "${env.NOOK_COMPOSE_FILE ?: 'docker-compose.prod.yml'}"
        DEPLOY_BRANCH = "${env.NOOK_DEPLOY_BRANCH ?: 'main'}"
    }

    stages {
        stage('Rust') {
            steps {
                script {
                    // A throwaway postgres for the life of this stage. The
                    // tenant-isolation tests skip themselves when there is no
                    // database — which meant CI reported "6 passed ... 0.00s"
                    // while executing none of them, for every build before this
                    // one. NOOK_REQUIRE_DB turns that silent skip into a failure
                    // so it cannot come back unnoticed.
                    def net = "nook-ci-net-${env.BUILD_NUMBER}"
                    def pg  = "nook-ci-pg-${env.BUILD_NUMBER}"
                    sh "docker network create ${net} >/dev/null 2>&1 || true"
                    sh "docker rm -f ${pg} >/dev/null 2>&1 || true"
                    sh "docker run -d --name ${pg} --network ${net} -e POSTGRES_USER=nook -e POSTGRES_PASSWORD=nook -e POSTGRES_DB=nook postgres:16-alpine >/dev/null"
                    sh "for i in \$(seq 1 60); do docker exec ${pg} pg_isready -U nook -d nook >/dev/null 2>&1 && exit 0; sleep 1; done; echo 'postgres never became ready' >&2; exit 1"
                    try {
                        // Same build dependencies as control.Dockerfile — curl
                        // is load-bearing: utoipa-swagger-ui downloads the UI
                        // bundle from its build script, and fails without it.
                        inImage(RUST_IMAGE,
                            "--network ${net} -e DATABASE_URL=postgres://nook:nook@${pg}:5432/nook -e NOOK_REQUIRE_DB=1",
                            'set -e; ' +
                            'apt-get update -qq && apt-get install -y -qq --no-install-recommends ' +
                            'pkg-config libssl-dev curl ca-certificates >/dev/null; ' +
                            'rustup component add rustfmt clippy >/dev/null 2>&1 || true; ' +
                            'cargo fmt --all --check && ' +
                            'cargo clippy --workspace --all-targets && ' +
                            'cargo test --workspace')
                    } finally {
                        sh "docker rm -f ${pg} >/dev/null 2>&1 || true"
                        sh "docker network rm ${net} >/dev/null 2>&1 || true"
                    }
                }
            }
        }

        stage('Frontend') {
            steps {
                script {
                    // CI=true: without a TTY pnpm refuses to reconcile a
                    // node_modules it didn't create, and aborts instead.
                    inImage(NODE_IMAGE, '-e CI=true',
                        'corepack enable && cd frontend && ' +
                        'pnpm install --frozen-lockfile && pnpm -r typecheck')
                }
            }
        }

        stage('Images') {
            steps {
                script {
                    // Built straight from the Dockerfiles rather than through
                    // compose: the compose file lives with the deployment, not
                    // in the repo, and this stage is about producing artefacts
                    // rather than describing a running system.
                    def sha = env.GIT_COMMIT ? env.GIT_COMMIT.take(12) : 'dev'
                    IMAGES.each { name, dockerfile ->
                        sh "docker build -f ${dockerfile} -t ${name}:${sha} ."
                    }
                    env.IMAGE_TAG = sha

                    if ((env.NOOK_REGISTRY ?: '') == '') {
                        echo 'NOOK_REGISTRY unset — images built but not pushed.'
                        return
                    }
                    // Push the immutable tag and move `latest`, so a deploy is
                    // a pull and a rollback is re-tagging a known-good sha.
                    withCredentials([usernamePassword(
                            credentialsId: env.NOOK_REGISTRY_CRED ?: 'registry',
                            usernameVariable: 'REG_USER',
                            passwordVariable: 'REG_PASS')]) {
                        sh '''
                            set -eu
                            echo "$REG_PASS" | docker login "${NOOK_REGISTRY%%/*}" \
                                -u "$REG_USER" --password-stdin
                        '''
                    }
                    IMAGES.each { name, _df ->
                        sh """
                            set -eu
                            docker tag ${name}:${sha} \$NOOK_REGISTRY/${name}:${sha}
                            docker tag ${name}:${sha} \$NOOK_REGISTRY/${name}:latest
                            docker push \$NOOK_REGISTRY/${name}:${sha}
                            docker push \$NOOK_REGISTRY/${name}:latest
                        """
                    }
                }
            }
        }

        stage('Deploy') {
            when {
                allOf {
                    expression { onDeployBranch() }
                    expression { (env.NOOK_DEPLOY_DIR ?: '') != '' }
                }
            }
            steps {
                script {
                    // The deploy checkout still tracks the commit, because the
                    // compose file and the schema deltas live there — but it no
                    // longer builds anything.
                    onDeployHost('git fetch --prune origin && ' +
                                 'git reset --hard "origin/$DEPLOY_BRANCH"')

                    // Schema first: the new binary may want columns the old
                    // schema lacks, and the deltas are additive, so the running
                    // version keeps working until it is replaced.
                    onDeployHost('if [ -n "${NOOK_PG_CONTAINER:-}" ]; then ' +
                                 './deploy/apply-schema.sh "$NOOK_PG_CONTAINER"; ' +
                                 'else echo "NOOK_PG_CONTAINER unset - skipping schema"; fi')

                    onDeployHost('docker compose -f "$COMPOSE_FILE" pull && ' +
                                 'docker compose -f "$COMPOSE_FILE" up -d')
                }
            }
        }

        stage('Health') {
            when {
                allOf {
                    expression { onDeployBranch() }
                    expression { (env.NOOK_HEALTH_URL ?: '') != '' }
                }
            }
            steps {
                // Fail the build if the thing we just deployed isn't answering.
                sh '''
                    set -eu
                    for i in $(seq 1 30); do
                        if curl -fsS "$NOOK_HEALTH_URL" >/dev/null 2>&1; then
                            echo "healthy after $((i * 2))s"
                            exit 0
                        fi
                        sleep 2
                    done
                    echo "still unhealthy after 60s"
                    exit 1
                '''
            }
        }
    }

    post {
        failure {
            echo 'Build failed — nothing was deployed unless the Deploy stage ran.'
        }
    }
}
