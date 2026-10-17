# This fixed app asset never restarts Windows, changes boot/firmware settings or terminates WSL.
param([switch]$Elevated)
$ErrorActionPreference = 'Stop'
if (-not $Elevated) {
    try {
        $arguments = @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', ('"' + $PSCommandPath + '"'), '-Elevated')
        $helper = Start-Process -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -ArgumentList $arguments -Verb RunAs -Wait -PassThru
        exit $helper.ExitCode
    } catch {
        [Console]::Error.WriteLine('Administrator approval was cancelled or denied by policy. Required Windows features were not enabled. ' + $_.Exception.Message)
        exit 5
    }
}
try {
    $processors = @(Get-CimInstance -ClassName Win32_Processor)
    if ($processors.Count -eq 0) { throw 'Unable to inspect processor virtualization support.' }
    $computer = Get-CimInstance -ClassName Win32_ComputerSystem
    if (-not $computer.HypervisorPresent -and ($processors | Where-Object { -not $_.VirtualizationFirmwareEnabled })) {
        [Console]::Error.WriteLine('Virtualization is unavailable in firmware. Enable CPU virtualization in firmware or contact your administrator; OMP Manager will not change firmware or boot settings.')
        exit 6
    }
    $restart = $false
    foreach ($name in @('Microsoft-Windows-Subsystem-Linux', 'VirtualMachinePlatform')) {
        $state = Get-WindowsOptionalFeature -Online -FeatureName $name
        if ($state.State -ne 'Enabled') {
            $result = Enable-WindowsOptionalFeature -Online -FeatureName $name -All -NoRestart
            $restart = $restart -or $result.RestartNeeded
        }
        if ($state.State -eq 'EnablePending') { $restart = $true }
    }
    if ($restart) { exit 3010 }
    exit 0
} catch {
    [Console]::Error.WriteLine('Windows feature activation failed (administrator policy or servicing error). ' + $_.Exception.Message)
    exit 7
}
