#!/bin/sh
# Fixed app-owned helper, never a host shell command. Do not enable shell tracing:
# .NET's documented /d:sonar.token argument is expanded only INSIDE this container.
# https://docs.sonarsource.com/sonarqube-server/analyzing-source-code/scanners/dotnet/using
set -eu
cd /tmp/omp-workspace
export HOME=/tmp/omp-home
export SONAR_USER_HOME=/tmp/omp-home/.sonar
export DOTNET_CLI_HOME=/tmp/omp-home
export DOTNET_CLI_TELEMETRY_OPTOUT=1
export DOTNET_NOLOGO=1
export GRADLE_USER_HOME=/tmp/omp-home/.gradle
export MAVEN_CONFIG=/tmp/omp-home/.m2
export MAVEN_OPTS="-Duser.home=/tmp/omp-home ${MAVEN_OPTS:-}"
if [ "${OMP_NPM_PREPARE:-false}" = true ]; then
    command -v npm >/dev/null 2>&1 || { echo 'The approved analysis image has no npm; select a compatible custom image.' >&2; exit 78; }
    npm ci --ignore-scripts
fi
case "$OMP_SCANNER" in
    cli)
        exec sonar-scanner "$@" "-Dsonar.host.url=$SONAR_HOST_URL" "-Dsonar.projectKey=$OMP_PROJECT_KEY" "-Dsonar.projectName=$OMP_PROJECT_NAME" -Dsonar.projectBaseDir=/tmp/omp-workspace -Dsonar.working.directory=/tmp/omp-workspace/.scannerwork -Dsonar.scm.disabled=true
        ;;
    maven)
        exec mvn -B "$@" -DskipTests -DskipITs clean verify org.sonarsource.scanner.maven:sonar-maven-plugin:5.8.0.7211:sonar "-Dsonar.host.url=$SONAR_HOST_URL" "-Dsonar.projectKey=$OMP_PROJECT_KEY" "-Dsonar.projectName=$OMP_PROJECT_NAME" -Dsonar.projectBaseDir=/tmp/omp-workspace -Dsonar.working.directory=/tmp/omp-workspace/.scannerwork -Dsonar.scm.disabled=true
        ;;
    gradle)
        if [ "$OMP_GRADLE_WRAPPER" = true ]; then set -- ./gradlew "$@"; else set -- gradle "$@"; fi
        if [ "$OMP_GRADLE_INIT" = true ]; then set -- "$@" --init-script /tmp/omp-quality/sonar.init.gradle; fi
        exec "$@" --no-daemon classes testClasses sonar "-Dsonar.host.url=$SONAR_HOST_URL" "-Dsonar.projectKey=$OMP_PROJECT_KEY" "-Dsonar.projectName=$OMP_PROJECT_NAME" -Dsonar.projectBaseDir=/tmp/omp-workspace -Dsonar.working.directory=/tmp/omp-workspace/.scannerwork -Dsonar.scm.disabled=true
        ;;
    dotnet)
        /opt/omp-sonarscanner/dotnet-sonarscanner begin "/k:$OMP_PROJECT_KEY" "/n:$OMP_PROJECT_NAME" "/d:sonar.host.url=$SONAR_HOST_URL" "/d:sonar.token=$SONAR_TOKEN" /d:sonar.scm.disabled=true
        dotnet build "$OMP_ENTRY_FILE" --no-incremental --disable-build-servers "$@"
        exec /opt/omp-sonarscanner/dotnet-sonarscanner end "/d:sonar.token=$SONAR_TOKEN"
        ;;
    *) echo 'Unsupported managed scanner' >&2; exit 78 ;;
esac
