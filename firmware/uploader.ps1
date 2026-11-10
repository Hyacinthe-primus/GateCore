param(
    [Parameter(Mandatory = $true)]
    [string]$Port,

    [Parameter(Mandatory = $false)]
    [string]$File,

    [switch]$Monitor,

    [int]$Baudrate = 2000000
)

$Fqbn = "esp32:esp32:esp32s3:FlashSize=16M,PSRAM=opi,PartitionScheme=custom"
$MonitorFqbn = "esp32:esp32:esp32s3"

# Serial Monitor only

if ($Monitor -and -not $File) {

    Write-Host ""
    Write-Host "Opening Serial Monitor..." -ForegroundColor Cyan
    Write-Host "Port     : $Port" -ForegroundColor Gray
    Write-Host "Baudrate : $Baudrate" -ForegroundColor Gray
    Write-Host ""

    arduino-cli monitor `
        -p $Port `
        -b $MonitorFqbn `
        --config "baudrate=$Baudrate"

    exit $LASTEXITCODE
}

# File required for compile/upload

if (-not $File) {
    Write-Host "Error: -File is required unless -Monitor is used alone." -ForegroundColor Red
    exit 1
}

if (-not (Test-Path $File)) {
    Write-Host "Error: File not found: $File" -ForegroundColor Red
    exit 1
}

# Compile

Write-Host ""
Write-Host "Compiling $File..." -ForegroundColor Cyan

arduino-cli compile `
    -p $Port `
    --fqbn $Fqbn `
    $File

if ($LASTEXITCODE -ne 0) {
    Write-Host ""
    Write-Host "Compilation failed." -ForegroundColor Red
    exit $LASTEXITCODE
}

Write-Host "Compilation successful." -ForegroundColor Green

# Upload

Write-Host ""
Write-Host "Uploading to $Port..." -ForegroundColor Cyan

arduino-cli upload `
    -p $Port `
    --fqbn $Fqbn `
    $File

if ($LASTEXITCODE -ne 0) {
    Write-Host ""
    Write-Host "Upload failed." -ForegroundColor Red
    exit $LASTEXITCODE
}

Write-Host "Upload successful." -ForegroundColor Green

# Serial Monitor after upload (optional)

if ($Monitor) {

    Write-Host ""
    Write-Host "Opening Serial Monitor..." -ForegroundColor Cyan

    arduino-cli monitor `
        -p $Port `
        -b $MonitorFqbn `
        --config "baudrate=$Baudrate"
}

Write-Host ""
Write-Host "Done." -ForegroundColor Green