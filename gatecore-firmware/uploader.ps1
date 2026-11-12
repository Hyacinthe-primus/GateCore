[CmdletBinding()]
param(
    [Alias('p')]
    [string]$Port,

    [Alias('f')]
    [string]$File,

    [Alias('m')]
    [switch]$Monitor,

    [switch]$MonitorOnly,

    [Alias('b', 'Baud')]
    [int]$Baudrate = 2000000,

    [switch]$Setup,

    [Alias('l')]
    [switch]$List,

    [Alias('h')]
    [switch]$Help
)

$Esp32IndexUrl = "https://espressif.github.io/arduino-esp32/package_esp32_index.json"
$Fqbn = "esp32:esp32:esp32s3:FlashSize=16M,PSRAM=opi,PartitionScheme=custom"
$MonitorFqbn = "esp32:esp32:esp32s3"
$Libs = @("Adafruit PN532", "LiquidCrystal I2C", "ArduinoJson@7.2.1")

$ScriptDir = $PSScriptRoot
if (-not $ScriptDir) {
    $ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
}
if (-not $File) {
    $File = Join-Path $ScriptDir "gatecore-firmware.ino"
}

function Show-Section([string]$Text) {
    Write-Host ""
    Write-Host $Text -ForegroundColor Cyan
}

function Show-Ok([string]$Text) {
    Write-Host $Text -ForegroundColor Green
}

function Stop-WithError([string]$Text, [int]$Code = 1) {
    Write-Host ""
    Write-Host $Text -ForegroundColor Red
    exit $Code
}

function Show-Usage {
    @"
Usage: uploader.ps1 [options]

  -Port, -p PORT       serial port, e.g. COM5
                       (auto-picked when exactly one candidate is present)
  -File, -f FILE       sketch to build (default: gatecore-firmware.ino next to this script)
  -Monitor, -m         open the serial monitor after a successful upload
  -MonitorOnly         open the serial monitor and skip compile and upload
  -Baudrate, -b N      monitor baud rate (default: 2000000)
  -Setup               install the ESP32 core and the libraries, then exit
  -List, -l            list the boards arduino-cli can see, then exit
  -Help, -h            show this help

Examples:
  .\uploader.ps1 -Setup
  .\uploader.ps1 -Port COM5 -Monitor
  .\uploader.ps1 -Port COM5 -MonitorOnly
"@ | Write-Host
}

function Get-CandidatePorts {
    $ports = @()

    try {
        $ports += @(
            Get-CimInstance -ClassName Win32_SerialPort -ErrorAction Stop |
                ForEach-Object { $_.DeviceID }
        )
    } catch {
        # WMI is unavailable; the .NET fallback below is enough.
    }

    try {
        $ports += @([System.IO.Ports.SerialPort]::GetPortNames())
    } catch {
        # Let the empty-list error explain what to do next.
    }

    return @(
        $ports |
            Where-Object { $_ -match '^COM\d+$' } |
            Sort-Object -Unique
    )
}

function Invoke-Monitor {
    Show-Section "Opening the serial monitor (Ctrl+C to quit)..."
    Write-Host "Port     : $Port" -ForegroundColor Gray
    Write-Host "Baudrate : $Baudrate" -ForegroundColor Gray
    Write-Host ""

    & arduino-cli monitor -p $Port -b $MonitorFqbn --config "baudrate=$Baudrate"
    $code = $LASTEXITCODE
    if ($null -eq $code) {
        $code = 0
    }
    return $code
}

if ($Help) {
    Show-Usage
    exit 0
}

if (-not (Get-Command arduino-cli -ErrorAction SilentlyContinue)) {
    Stop-WithError @"
arduino-cli not found. Install it from https://arduino.github.io/arduino-cli/latest/installation/
On Windows, download the zip from the releases page and add the folder to PATH.
"@
}

if ($Setup) {
    Show-Section "Registering the ESP32 board index..."
    & arduino-cli config init | Out-Null

    $config = (& arduino-cli config dump 2>$null | Out-String)
    if ($config -notmatch 'package_esp32_index\.json') {
        & arduino-cli config add board_manager.additional_urls $Esp32IndexUrl
        if ($LASTEXITCODE -ne 0) {
            Stop-WithError "Could not edit the arduino-cli config."
        }
    }

    & arduino-cli core update-index
    if ($LASTEXITCODE -ne 0) {
        Stop-WithError "core update-index failed."
    }

    Show-Section "Installing the ESP32 core (large download, be patient)..."
    & arduino-cli core install esp32:esp32
    if ($LASTEXITCODE -ne 0) {
        Stop-WithError "ESP32 core install failed."
    }

    Show-Section "Installing libraries..."
    & arduino-cli lib install @Libs
    if ($LASTEXITCODE -ne 0) {
        Stop-WithError "Library install failed."
    }

    Show-Ok "Setup done."
    exit 0
}

if ($List) {
    & arduino-cli board list
    $code = $LASTEXITCODE
    if ($null -eq $code) {
        $code = 0
    }
    exit $code
}

if (-not $Port) {
    $candidates = @(Get-CandidatePorts)
    if ($candidates.Count -eq 1) {
        $Port = $candidates[0]
        Write-Host "Using $Port"
    } elseif ($candidates.Count -eq 0) {
        Stop-WithError "No serial port found. Plug the board in, or pass -Port. (-List shows what arduino-cli sees.)"
    } else {
        Write-Host "Several serial ports found:" -ForegroundColor Red
        foreach ($candidate in $candidates) {
            Write-Host "  $candidate" -ForegroundColor Red
        }
        Stop-WithError "Pick one with -Port."
    }
}

if ($MonitorOnly) {
    exit (Invoke-Monitor)
}

if (-not (Test-Path -LiteralPath $File)) {
    Stop-WithError "File not found: $File"
}

Show-Section "Compiling $File..."
& arduino-cli compile --fqbn $Fqbn $File
if ($LASTEXITCODE -ne 0) {
    $code = $LASTEXITCODE
    Stop-WithError "Compilation failed. If the ESP32 core or a library is missing, run: .\gatecore-firmware\uploader.ps1 -Setup" $code
}
Show-Ok "Compilation successful."

Show-Section "Uploading to $Port..."
& arduino-cli upload -p $Port --fqbn $Fqbn $File
if ($LASTEXITCODE -ne 0) {
    $code = $LASTEXITCODE
    Stop-WithError "Upload failed. Some boards need the BOOT button held while the upload starts." $code
}
Show-Ok "Upload successful."

if ($Monitor) {
    $code = Invoke-Monitor
    if ($code -ne 0) {
        exit $code
    }
}

Write-Host ""
Show-Ok "Done."
