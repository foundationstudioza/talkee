# Runs the Kotlin known-answer checks (SelfCheck.kt) on the desktop JVM.
#
# Why this exists: `junit` is not in this machine's Gradle cache and there is no network
# to fetch it, so the Android unit-test source set cannot be used. The checks themselves
# ship inside the app and also run on the device at startup in a debug build; this script
# is the fast loop for changing them.
#
# Usage:  powershell -ExecutionPolicy Bypass -File app\tools\run-kotlin-selftest.ps1

$ErrorActionPreference = 'Stop'

$app = Split-Path -Parent $PSScriptRoot
$source = Join-Path $app 'android\app\src\main\kotlin\net\talkee\talkee_app'
$selftest = Join-Path $app 'android\app\src\selftest\kotlin\net\talkee\talkee_app\SelfTestMain.kt'
$out = Join-Path $env:TEMP 'talkee-selftest-classes'

# The pure files: no android imports, so they compile and run on a plain JVM. This list
# is explicit on purpose — adding an Android import to one of them should break the
# script loudly rather than silently dropping the file from the check.
$pure = @('Wire.kt', 'Crypto.kt', 'Encoding.kt', 'Pairing.kt', 'Control.kt', 'CallWatch.kt', 'SelfCheck.kt')

$cache = 'E:\programming\java\gradle\caches\modules-2\files-2.1'
if (-not (Test-Path $cache)) { throw "Gradle cache not found at $cache" }

function Find-Jar([string]$group, [string]$name) {
    $jar = Find-JarOptional $group $name
    if (-not $jar) { throw "no $name under $group in the Gradle cache" }
    return $jar
}

function Find-JarOptional([string]$group, [string]$name) {
    $root = Join-Path $cache $group
    if (-not (Test-Path $root)) { return $null }
    # Scoped to one group directory: recursing the whole cache can die on an
    # unreadable directory belonging to another tool, which then looks like
    # "the jar is missing".
    $jar = Get-ChildItem $root -Recurse -Filter $name -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $jar) { return $null }
    return $jar.FullName
}

$compiler = Find-Jar 'org.jetbrains.kotlin\kotlin-compiler-embeddable' 'kotlin-compiler-embeddable-2.2.21.jar'
$stdlib = Find-Jar 'org.jetbrains.kotlin\kotlin-stdlib' 'kotlin-stdlib-2.2.21.jar'

# The embeddable compiler is not a fat jar: it needs its own runtime pieces on the
# classpath of the *compiler process*. All four are pinned to one Kotlin version
# because a mismatched `kotlin-reflect` crashes the 2.4.0 backend while generating
# bytecode (it did, on `CallWatch.reason`), and a tool that fails on innocent code is
# worse than no tool. The app itself is still compiled by Gradle with 2.4.0.
$launcher = @(
    $compiler,
    $stdlib,
    (Find-Jar 'org.jetbrains.kotlin\kotlin-script-runtime' 'kotlin-script-runtime-2.2.21.jar'),
    (Find-Jar 'org.jetbrains.kotlin\kotlin-daemon-embeddable' 'kotlin-daemon-embeddable-2.2.21.jar'),
    (Find-Jar 'org.jetbrains.kotlin\kotlin-reflect' 'kotlin-reflect-2.2.21.jar')
)

# Optional: the compiler only needs these for features this script does not use, and a
# Gradle cache GC in another window can remove them, which would otherwise look like a
# broken self-check.
foreach ($optional in @(
        @('org.jetbrains\annotations', 'annotations-*.jar'),
        @('org.jetbrains.intellij.deps\trove4j', 'trove4j-*.jar'),
        @('org.jetbrains.kotlinx\kotlinx-coroutines-core-jvm', 'kotlinx-coroutines-core-jvm-*.jar')
    )) {
    $jar = Find-JarOptional $optional[0] $optional[1]
    if ($jar) { $launcher += $jar }
}

$files = @()
foreach ($file in $pure) {
    $path = Join-Path $source $file
    if (-not (Test-Path $path)) { throw "missing pure source file $file" }
    $files += $path
}
$files += $selftest

Remove-Item $out -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "compiling $($files.Count) files with $compiler"
& java -cp ($launcher -join ';') org.jetbrains.kotlin.cli.jvm.K2JVMCompiler -no-stdlib -nowarn -cp $stdlib -d $out @files
if ($LASTEXITCODE -ne 0) { throw "the Kotlin compiler failed with exit code $LASTEXITCODE" }

& java -cp "$out;$stdlib" net.talkee.talkee_app.SelfTestMainKt
if ($LASTEXITCODE -ne 0) { throw "the self-check failed with exit code $LASTEXITCODE" }
