@echo off
setlocal DisableDelayedExpansion
title File Backup v0.2.0-alpha - by godblessmerica
mode con: cols=58 lines=28
set "FILEBACKUP_BOOTSTRAP=%~f0"
powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; $self=$env:FILEBACKUP_BOOTSTRAP; $text=[IO.File]::ReadAllText($self); $root=[IO.Path]::GetDirectoryName($self); foreach($name in @('manager.ps1','config.json')) { $target=Join-Path $root $name; if(-not (Test-Path -LiteralPath $target)) { $pattern='(?ms)^'+[regex]::Escape('### FILEBACKUP:BEGIN '+$name)+'\r?\n(.*?)\r?\n'+[regex]::Escape('### FILEBACKUP:END '+$name)+'\r?$'; $match=[regex]::Match($text,$pattern); if(-not $match.Success) { throw ('Missing embedded file: '+$name) }; [byte[]]$bytes=[Text.Encoding]::UTF8.GetPreamble()+[Text.Encoding]::UTF8.GetBytes($match.Groups[1].Value+[Environment]::NewLine); $stream=[IO.File]::Open($target,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write); try { $stream.Write($bytes,0,$bytes.Length) } finally { $stream.Dispose() } } }; & (Join-Path $root 'manager.ps1')"
exit /b %errorlevel%

### FILEBACKUP:BEGIN manager.ps1
param(
    [string]$Source,
    [string]$BackupRoot = (Join-Path $PSScriptRoot '.backups'),
    [string]$ExistingBackupName,
    [string]$LogDirectory = (Join-Path $PSScriptRoot '.logs'),
    [switch]$NonInteractive
)

$ErrorActionPreference = 'Stop'
$script:SessionLog = $null
$script:ProgressLastDraw = [datetime]::MinValue
$script:ProgressRow = $null
$script:Settings = $null

function Get-BackupSettings([string]$ConfigPath = (Join-Path $PSScriptRoot 'config.json'), [switch]$Refresh) {
    if (-not $Refresh -and $script:Settings -and $ConfigPath -eq (Join-Path $PSScriptRoot 'config.json')) { return $script:Settings }
    $settings = @{
        BackupRoot = '.backups'; LogDirectory = '.logs'
        CompressionLevel = 5; LogCompressionLevel = 6
        DateFormat = '{year}-{month}-{day}_{hour}-{minute}-{second}'
        ExcludeFiles = @(); ExcludeFolders = @(); OverwriteExistingFiles = $true
        CompletionDelaySeconds = 2; CopyRetries = 3; RetryDelaySeconds = 5
    }
    if (Test-Path -LiteralPath $ConfigPath) {
        $config = Get-Content -LiteralPath $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($null -eq $config -or $config -isnot [pscustomobject]) { throw 'config.json must contain a JSON object.' }
        foreach ($property in $config.PSObject.Properties) {
            if ($settings.ContainsKey($property.Name)) { $settings[$property.Name] = $property.Value }
        }
    }
    foreach ($name in 'CompressionLevel', 'LogCompressionLevel') {
        if ($settings[$name] -isnot [int] -and $settings[$name] -isnot [long] -or
            $settings[$name] -notin 0, 1, 3, 5, 6, 7, 9) { throw "$name must be 0, 1, 3, 5, 6, 7, or 9." }
    }
    foreach ($name in 'CompletionDelaySeconds', 'CopyRetries', 'RetryDelaySeconds') {
        if ($settings[$name] -isnot [int] -and $settings[$name] -isnot [long] -or
            $settings[$name] -lt 0 -or $settings[$name] -gt [int]::MaxValue) { throw "$name must be a nonnegative integer." }
    }
    foreach ($name in 'BackupRoot', 'LogDirectory', 'DateFormat') {
        if ($settings[$name] -isnot [string] -or -not $settings[$name].Trim()) { throw "$name must be a nonempty string." }
    }
    if ($settings.OverwriteExistingFiles -isnot [bool]) { throw 'OverwriteExistingFiles must be true or false.' }
    foreach ($name in 'ExcludeFiles', 'ExcludeFolders') {
        if ($settings[$name] -isnot [array]) { throw "$name must be an array of wildcard patterns." }
        foreach ($pattern in $settings[$name]) {
            if ($pattern -isnot [string] -or -not $pattern.Trim() -or $pattern -match '[\r\n]') {
                throw "$name must contain nonempty wildcard strings."
            }
        }
    }
    return $settings
}

function Resolve-BackupSettingPath([string]$Path) {
    if (-not [IO.Path]::IsPathRooted($Path)) { $Path = Join-Path $PSScriptRoot $Path }
    return [IO.Path]::GetFullPath($Path)
}

function Write-BackupText([string]$Text = '', [ConsoleColor]$ForegroundColor, [switch]$NoNewline) {
    $options = @{}
    if ($PSBoundParameters.ContainsKey('ForegroundColor')) { $options.ForegroundColor = $ForegroundColor }
    while ($Text.Length -gt 57) {
        $split = $Text.LastIndexOf(' ', 56, 57)
        if ($split -lt 1) { $split = 57 }
        Write-Host $Text.Substring(0, $split) @options
        $Text = $Text.Substring($split).TrimStart()
    }
    Write-Host $Text @options -NoNewline:$NoNewline
}

function Get-BackupProgressText([string]$Activity, [int]$Percent, [int]$Width) {
    $barWidth = [Math]::Max(1, [Math]::Min(24, $Width - 24))
    if ($Percent -lt 0) {
        $bar = '-' * $barWidth
        $status = 'Working'
    } else {
        $Percent = [Math]::Min(100, $Percent)
        $filled = [int][Math]::Floor($barWidth * $Percent / 100)
        $bar = ('=' * $filled) + ('-' * ($barWidth - $filled))
        $status = "$Percent%"
    }
    $text = "[$bar] $status $Activity"
    if ($text.Length -gt $Width) { $text = $text.Substring(0, $Width) }
    return $text.PadRight($Width)
}

function Get-ToolProgressPercent([string]$Line) {
    if ($Line -match '^\s*(\d{1,3})(?:[.,]\d+)?%') {
        return [Math]::Min(100, [int]$Matches[1])
    }
    return -1
}

function Show-BackupProgress([string]$Activity, [int]$Percent = -1, [ConsoleColor]$Color = 'Cyan') {
    if ([Console]::IsOutputRedirected) { return }
    if (([datetime]::UtcNow - $script:ProgressLastDraw).TotalMilliseconds -lt 100 -and $Color -eq 'Cyan') { return }
    try {
        $width = [Math]::Min(57, [Math]::Min([Console]::WindowWidth, [Console]::BufferWidth) - 1)
        if ($width -lt 1) { return }
        if ($null -eq $script:ProgressRow) {
            $script:ProgressRow = [Console]::CursorTop
            [Console]::WriteLine()
        }
        $left = [Console]::CursorLeft
        $top = [Console]::CursorTop
        $oldColor = [Console]::ForegroundColor
        try {
            [Console]::SetCursorPosition(0, $script:ProgressRow)
            [Console]::ForegroundColor = $Color
            [Console]::Write((Get-BackupProgressText $Activity $Percent $width))
        } finally {
            [Console]::ForegroundColor = $oldColor
            [Console]::SetCursorPosition($left, $top)
        }
        $script:ProgressLastDraw = [datetime]::UtcNow
    } catch {
        # A resized/unavailable console must not interrupt the backup.
    }
}

function Test-WithinPath([string]$Path, [string]$Parent) {
    $parentPath = $Parent.TrimEnd('\')
    return $Path.TrimEnd('\').Equals($parentPath, [StringComparison]::OrdinalIgnoreCase) -or
        $Path.StartsWith($parentPath + '\', [StringComparison]::OrdinalIgnoreCase)
}

function Assert-NoReparseAncestor([string]$Path) {
    while ($Path) {
        if (Test-Path -LiteralPath $Path) {
            $item = Get-Item -LiteralPath $Path -Force
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "Linked paths are not supported: $Path"
            }
        }
        $Path = Split-Path -Path $Path -Parent
    }
}

function Assert-BackupName([string]$Name) {
    if (-not $Name -or $Name -in '.', '..', '.logs' -or
        $Name.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0 -or
        $Name -match '[. ]$|^(CON|PRN|AUX|NUL|COM[0-9]|LPT[0-9])(?:\.|$)') {
        throw 'Enter one valid backup name, without a path.'
    }
}

function Get-ExistingBackup([string]$Root, [string]$Name, [switch]$AllowArchive) {
    Assert-BackupName $Name
    $destination = Join-Path ([IO.Path]::GetFullPath($Root)) $Name
    if (-not (Test-Path -LiteralPath $destination)) { throw 'Selected backup does not exist.' }
    $item = Get-Item -LiteralPath $destination -Force
    if (-not $item.PSIsContainer -and (-not $AllowArchive -or $item.Extension -ine '.zip')) {
        throw 'Select a backup folder or, for rename/delete, a ZIP backup.'
    }
    Assert-NoReparseAncestor $destination
    $links = @()
    if ($item.PSIsContainer) {
        $links = Get-ChildItem -LiteralPath $destination -Recurse -Force |
            Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint }
    }
    if ($links) { throw 'Selected backup contains linked files or folders.' }
    return $destination
}

function New-BackupLogPath {
    if ($script:SessionLog) { return $script:SessionLog }
    Assert-NoReparseAncestor $LogDirectory
    return (New-DatedBackupPath $LogDirectory '' '.log')
}

function Get-BackupDateFormat([string]$ConfigPath = (Join-Path $PSScriptRoot 'config.json')) {
    return (Get-BackupSettings $ConfigPath).DateFormat
}

function Get-BackupDateStamp([string]$ConfigPath = (Join-Path $PSScriptRoot 'config.json')) {
    $format = Get-BackupDateFormat $ConfigPath
    $now = Get-Date
    $stamp = $format.Replace('{year}', $now.ToString('yyyy')).Replace('{month}', $now.ToString('MM')).Replace('{day}', $now.ToString('dd')).Replace('{time}', $now.ToString('HHmmss'))
    $stamp = $stamp.Replace('{hour}', $now.ToString('HH')).Replace('{minute}', $now.ToString('mm')).Replace('{second}', $now.ToString('ss'))
    if ($stamp -match '[{}]' -or $stamp.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0) {
        throw 'DateFormat supports {year}, {month}, {day}, {hour}, {minute}, {second}, {time}, and filename-safe separators.'
    }
    return $stamp
}

function New-DatedBackupPath([string]$Root, [string]$Name, [string]$Extension = '') {
    $stamp = Get-BackupDateStamp
    while ($true) {
        $prefix = if ($Name) { $Name + '-' } else { '' }
        $path = Join-Path ([IO.Path]::GetFullPath($Root)) ($prefix + $stamp + $Extension)
        if (-not (Test-Path -LiteralPath $path) -and
            -not ($Extension -eq '.log' -and (Test-Path -LiteralPath ($path + '.gz')))) {
            return $path
        }
        # Wait for a fresh timestamp instead of adding IDs or overwriting an existing backup/log.
        Start-Sleep -Milliseconds 1000
        $nextStamp = Get-BackupDateStamp
        if ($nextStamp -eq $stamp) { throw 'This name already exists. Include {second} or {time} in DateFormat to create another backup/log.' }
        $stamp = $nextStamp
    }
}

function Restore-BackupLog([string]$Path) {
    if ((Test-Path -LiteralPath $Path) -or -not (Test-Path -LiteralPath ($Path + '.gz'))) { return }
    $partial = $Path + '.restore.partial'
    Assert-NoReparseAncestor ($Path + '.gz')
    Assert-NoReparseAncestor $partial
    $inputStream = $null; $gzip = $null; $outputStream = $null; $created = $false
    try {
        $inputStream = [IO.File]::OpenRead($Path + '.gz')
        $gzip = [IO.Compression.GZipStream]::new($inputStream, [IO.Compression.CompressionMode]::Decompress)
        $outputStream = [IO.File]::Open($partial, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
        $created = $true
        $gzip.CopyTo($outputStream)
        $outputStream.Dispose(); $outputStream = $null
        [IO.File]::Move($partial, $Path)
    } finally {
        if ($outputStream) { $outputStream.Dispose() }
        if ($gzip) { $gzip.Dispose() }
        if ($inputStream) { $inputStream.Dispose() }
        if ($created -and (Test-Path -LiteralPath $partial)) { Remove-Item -LiteralPath $partial -Force }
    }
}

function Get-SavedLogPath([string]$Path) {
    if (Test-Path -LiteralPath $Path) { return $Path }
    return ($Path + '.gz')
}

function Write-BackupEvent($Plan, [string]$Event, [string]$Details = '') {
    $directory = Split-Path -Path $Plan.Log -Parent
    Assert-NoReparseAncestor $Plan.Log
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    Restore-BackupLog $Plan.Log
    $timestamp = [DateTimeOffset]::Now.ToString('yyyy-MM-dd HH:mm:ss.fff zzz')
    $detailsLine = $Details.Replace("`r", ' ').Replace("`n", ' ')
    $line = "$timestamp | $Event | Source: $($Plan.Source) | Backup: $($Plan.Destination) | $detailsLine | Log: $($Plan.Log)"
    Add-Content -LiteralPath $Plan.Log -Value $line -Encoding Unicode
}

function Start-BackupSession([string]$Root, [string]$Logs) {
    foreach ($directory in @($Root, $Logs)) {
        Assert-NoReparseAncestor $directory
        if (Test-Path -LiteralPath $directory) {
            if (-not (Test-Path -LiteralPath $directory -PathType Container)) {
                throw "Required folder path is occupied by a file: $directory"
            }
        } else { [IO.Directory]::CreateDirectory($directory) | Out-Null }
    }
    $logPath = New-DatedBackupPath $Logs '' '.log'
    $file = [IO.File]::Open($logPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
    $file.Dispose()
    $script:SessionLog = $logPath
    $session = [pscustomobject]@{ Source = 'FileBackup'; Destination = $Root; Log = $script:SessionLog }
    Write-BackupEvent $session 'RUN_STARTED'
    Save-BackupSessionLog -WarningOnly
}

function Save-BackupSessionLog([switch]$Final, [switch]$WarningOnly) {
    if (-not $script:SessionLog) { return }
    $log = $script:SessionLog
    $session = [pscustomobject]@{ Source = 'FileBackup'; Destination = $BackupRoot; Log = $log }
    if ($Final) { Write-BackupEvent $session 'RUN_FINISHED' }
    $partial = $log + '.gz.partial'
    try {
        Assert-NoReparseAncestor ($log + '.gz')
        Assert-NoReparseAncestor $partial
        $sevenZip = Get-SevenZip
        $level = (Get-BackupSettings).LogCompressionLevel
        & $sevenZip a -tgzip "-mx=$level" -spd $partial -- $log | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Log compression failed.' }
        & $sevenZip t -tgzip $partial | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Compressed log verification failed.' }
        if (Test-Path -LiteralPath ($log + '.gz')) {
            [IO.File]::Replace($partial, $log + '.gz', [System.Management.Automation.Language.NullString]::Value)
        } else { [IO.File]::Move($partial, $log + '.gz') }
        Remove-Item -LiteralPath $log -Force
        if ($Final) {
            Write-BackupText "Session log: $log.gz"
        }
    } catch {
        Write-BackupEvent $session 'LOG_COMPRESSION_FAILED' $_.Exception.Message
        if ($WarningOnly) {
            Write-BackupText "Log compression failed; uncompressed log retained: $log. $($_.Exception.Message)" -ForegroundColor Red
        } else { throw }
    } finally {
        if (Test-Path -LiteralPath $partial) { Remove-Item -LiteralPath $partial -Force }
        if ($Final) { $script:SessionLog = $null }
    }
}

function New-BackupActionPlan([string]$Root, [string]$Name, [string]$NewName = '') {
    $sourcePath = Get-ExistingBackup $Root $Name -AllowArchive
    $destination = $sourcePath
    $operation = 'BACKUP_DELETE'
    if ($NewName) {
        Assert-BackupName $NewName
        if ([IO.Path]::GetExtension($sourcePath) -ieq '.zip' -and
            (Test-Path -LiteralPath $sourcePath -PathType Leaf) -and [IO.Path]::GetExtension($NewName) -ine '.zip') {
            throw 'A renamed ZIP backup must keep its .zip extension.'
        }
        $destination = Join-Path ([IO.Path]::GetFullPath($Root)) $NewName
        if (Test-Path -LiteralPath $destination) { throw 'The new backup name already exists.' }
        $operation = 'BACKUP_RENAME'
    }
    return [pscustomobject]@{
        Source = $sourcePath; Destination = $destination; Root = [IO.Path]::GetFullPath($Root)
        Log = (New-BackupLogPath); Operation = $operation
    }
}

function Invoke-BackupAction($Plan) {
    Write-BackupEvent $Plan ($Plan.Operation + '_STARTED')
    try {
        # Resolve the selected immediate child again before a rename or recursive deletion.
        $sourcePath = Get-ExistingBackup $Plan.Root (Split-Path -Path $Plan.Source -Leaf) -AllowArchive
        if (-not $sourcePath.Equals($Plan.Source, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Backup path changed; action cancelled.'
        }
        if ($Plan.Operation -eq 'BACKUP_RENAME') {
            $target = [IO.Path]::GetFullPath($Plan.Destination)
            if (-not (Split-Path -Path $target -Parent).Equals($Plan.Root.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase)) {
                throw 'Rename destination must stay directly inside the backup folder.'
            }
            $newName = Split-Path -Path $target -Leaf
            Assert-BackupName $newName
            if (Test-Path -LiteralPath $target) { throw 'The new backup name already exists.' }
            Rename-Item -LiteralPath $sourcePath -NewName $newName
            Write-BackupEvent $Plan 'BACKUP_RENAMED' 'Old and new paths are recorded above.'
        } elseif ($Plan.Operation -eq 'BACKUP_DELETE') {
            if (-not (Test-WithinPath $sourcePath $Plan.Root) -or $sourcePath.Equals($Plan.Root, [StringComparison]::OrdinalIgnoreCase)) {
                throw 'Deletion target must be a backup inside the backup folder.'
            }
            Remove-Item -LiteralPath $sourcePath -Recurse -Force
            Write-BackupEvent $Plan 'BACKUP_DELETED' 'Permanently deleted the selected backup.'
        } else { throw 'Invalid backup action.' }
    } catch {
        Write-BackupEvent $Plan ($Plan.Operation + '_FAILED') $_.Exception.Message
        throw
    } finally {
        Save-BackupSessionLog -WarningOnly
    }
}

function Get-NewBackupPath([string]$Root, [string]$Name) {
    Assert-BackupName $Name
    $path = Join-Path ([IO.Path]::GetFullPath($Root)) $Name
    Assert-NoReparseAncestor $path
    if (Test-Path -LiteralPath $path) {
        throw 'Backup name already exists. Use Add to existing backup instead of creating a new backup, or choose another name.'
    }
    return $path
}

function New-BackupPlan([string]$SourcePath, [string]$Root, [string]$ExistingName, [string]$Extension = '', [switch]$CreateNew) {
    $SourcePath = $SourcePath.Trim()
    if ($SourcePath.StartsWith('"') -and $SourcePath.EndsWith('"')) {
        $SourcePath = $SourcePath.Substring(1, $SourcePath.Length - 2)
    }
    if (-not $SourcePath) { throw 'Enter a source file or folder.' }
    $item = Get-Item -LiteralPath $SourcePath -Force
    if ($item.PSProvider.Name -ne 'FileSystem') { throw 'Source must be a filesystem path.' }
    $rootPath = [IO.Path]::GetFullPath($Root)
    Assert-NoReparseAncestor $item.FullName
    Assert-NoReparseAncestor $rootPath
    if (Test-WithinPath $item.FullName $rootPath) { throw 'Cannot back up the backup directory or its contents.' }
    if ($item.PSIsContainer -and (Test-WithinPath $rootPath $item.FullName)) {
        throw 'Backup destination cannot be inside the source folder.'
    }
    if ($item.PSIsContainer -and (Test-WithinPath ([IO.Path]::GetFullPath($LogDirectory)) $item.FullName)) {
        throw 'Log directory cannot be inside the source folder.'
    }

    $operation = 'BACKUP_CREATE'
    if ($ExistingName) {
        if ($CreateNew) {
            $destination = Get-NewBackupPath $rootPath $ExistingName
        } else {
            $destination = Get-ExistingBackup $rootPath $ExistingName
            $operation = 'BACKUP_UPDATE'
        }
    } else {
        $name = $item.Name
        if (-not $name -or $name.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0) { $name = 'drive' }
        $destination = Get-NewBackupPath $rootPath ($name + $Extension)
    }
    [pscustomobject]@{
        Source = $item.FullName
        IsDirectory = $item.PSIsContainer
        Destination = $destination
        Log = (New-BackupLogPath)
        Operation = $operation
    }
}

function Get-BackupCompletionMessage($Plan) {
    if ($Plan.Operation -eq 'BACKUP_UPDATE') { return 'Successfully added files' }
    if ($Plan.Operation -eq 'BACKUP_CREATE') { return 'Successfully uploaded files' }
    return 'Successfully created new ZIP'
}

function Invoke-Backup($Plan) {
    Write-BackupEvent $Plan ($Plan.Operation + '_STARTED')
    try {
        if ($Plan.Operation -eq 'BACKUP_CREATE' -and (Test-Path -LiteralPath $Plan.Destination)) {
            throw 'Backup destination already exists. Create a new backup or explicitly select an update.'
        }
        [IO.Directory]::CreateDirectory($Plan.Destination) | Out-Null
        if ($Plan.IsDirectory) {
            $arguments = @($Plan.Source, $Plan.Destination, '/E')
        } else {
            $arguments = @((Split-Path -Path $Plan.Source -Parent), $Plan.Destination,
                (Split-Path -Path $Plan.Source -Leaf))
        }
        # Linked directories are excluded; symbolic links are copied without following their targets.
        $settings = Get-BackupSettings
        $arguments += @('/Z', "/R:$($settings.CopyRetries)", "/W:$($settings.RetryDelaySeconds)", '/XJ', '/SL', '/COPY:DAT', '/DCOPY:T', '/TEE', "/UNILOG+:$($Plan.Log)")
        if (-not $settings.OverwriteExistingFiles) { $arguments += @('/XC', '/XN', '/XO') }
        if ($settings.ExcludeFiles.Count) { $arguments += @('/XF') + $settings.ExcludeFiles }
        if ($settings.ExcludeFolders.Count) { $arguments += @('/XD') + $settings.ExcludeFolders }
        $activity = if ($Plan.Operation -eq 'BACKUP_UPDATE') { 'Adding current file' } else { 'Uploading current file' }
        Show-BackupProgress $activity
        & robocopy.exe @arguments | ForEach-Object {
            $percent = Get-ToolProgressPercent ([string]$_)
            if ($percent -ge 0) { Show-BackupProgress $activity $percent }
        }
        $copyCode = $LASTEXITCODE
        if ($copyCode -ge 8) {
            throw "Backup incomplete. Robocopy exit code: $copyCode. Log: $($Plan.Log)"
        }
        $event = 'BACKUP_CREATED'
        $details = "Robocopy exit code: $copyCode."
        if ($Plan.Operation -eq 'BACKUP_UPDATE') {
            $event = 'BACKUP_UNCHANGED'
            if ($copyCode -band 1) {
                $event = 'BACKUP_UPDATED'
                $details += " Files copied. OverwriteExistingFiles: $($settings.OverwriteExistingFiles). See file details in the operation log."
            }
        }
        Write-BackupEvent $Plan $event $details
        $completed = if ($Plan.Operation -eq 'BACKUP_UPDATE') { 'Files added' } else { 'Files uploaded' }
        Show-BackupProgress $completed 100 Green
    } catch {
        Show-BackupProgress 'Backup failed' -1 Red
        Write-BackupEvent $Plan 'BACKUP_FAILED' $_.Exception.Message
        throw
    } finally {
        Save-BackupSessionLog -WarningOnly
    }
}

function Get-ZipCompressionLevel([string]$ConfigPath = (Join-Path $PSScriptRoot 'config.json')) {
    return [int](Get-BackupSettings $ConfigPath).CompressionLevel
}

function Get-SevenZip {
    $command = Get-Command 7z.exe -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    foreach ($directory in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) {
        if ($directory) {
            $candidate = Join-Path $directory '7-Zip\7z.exe'
            if (Test-Path -LiteralPath $candidate -PathType Leaf) { return $candidate }
        }
    }
    throw 'ZIP backups require 7-Zip. Install it or add 7z.exe to PATH.'
}

function New-ZipPlan([string]$SourcePath, [string]$Root, [string]$ExistingName) {
    if ($ExistingName) {
        $sourceFolder = Get-ExistingBackup $Root $ExistingName
        $plan = [pscustomobject]@{
            Source = $sourceFolder
            IsDirectory = $true
            Destination = (Get-NewBackupPath $Root ($ExistingName + '.zip'))
            Log = (New-BackupLogPath)
            Operation = 'ZIP_COMPRESS_EXISTING'
        }
    } else {
        $plan = New-BackupPlan $SourcePath $Root '' '.zip'
    }
    if ($plan.IsDirectory) {
        if (Test-WithinPath ([IO.Path]::GetFullPath($LogDirectory)) $plan.Source) { throw 'Log directory cannot be inside the source folder.' }
        $links = Get-ChildItem -LiteralPath $plan.Source -Recurse -Force |
            Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint }
        if ($links) { throw 'ZIP source contains linked files or folders. Choose a source without links.' }
    }
    if (-not $ExistingName) { $plan.Operation = 'ZIP_CREATE' }
    return $plan
}

function Invoke-ZipBackup($Plan, [ValidateSet(0, 1, 3, 5, 6, 7, 9)][int]$Level = 6) {
    $partial = $Plan.Destination + '.' + [guid]::NewGuid().ToString('N') + '.partial'
    Write-BackupEvent $Plan ($Plan.Operation + '_STARTED') "Compression level: $Level."
    try {
        $sevenZip = Get-SevenZip
        Assert-NoReparseAncestor $Plan.Destination
        if (Test-Path -LiteralPath $Plan.Destination) { throw 'ZIP destination already exists.' }
        [IO.Directory]::CreateDirectory((Split-Path -Path $Plan.Destination -Parent)) | Out-Null
        if ($Plan.IsDirectory) {
            $workingDirectory = $Plan.Source
            $inputPath = '.'
        } else {
            $workingDirectory = Split-Path -Path $Plan.Source -Parent
            $inputPath = '.\' + (Split-Path -Path $Plan.Source -Leaf)
        }
        $settings = Get-BackupSettings
        $exclusions = @()
        $items = @()
        if ($settings.ExcludeFiles.Count -or $settings.ExcludeFolders.Count) {
            $items = if ($Plan.IsDirectory) { Get-ChildItem -LiteralPath $Plan.Source -Recurse -Force }
                     else { Get-Item -LiteralPath $Plan.Source -Force }
        }
        foreach ($item in $items) {
            $relative = $item.FullName.Substring($workingDirectory.TrimEnd('\').Length + 1)
            $patterns = if ($item.PSIsContainer) { $settings.ExcludeFolders } else { $settings.ExcludeFiles }
            foreach ($pattern in $patterns) {
                if ($item.Name -like $pattern -or $relative -like $pattern -or $item.FullName -like $pattern) {
                    $exclusions += ('-x!' + $relative)
                    break
                }
            }
        }
        Push-Location -LiteralPath $workingDirectory
        try {
            $method = if ($Level -eq 0) { 'Copy' } else { 'Deflate' }
            Show-BackupProgress 'Creating ZIP'
            & $sevenZip a -tzip "-mm=$method" "-mx=$Level" -spd $partial -bsp1 @exclusions -- $inputPath 2>&1 |
                Tee-Object -FilePath $Plan.Log -Append | ForEach-Object {
                    $percent = Get-ToolProgressPercent ([string]$_)
                    if ($percent -ge 0) { Show-BackupProgress 'Creating ZIP' $percent }
                }
            if ($LASTEXITCODE -ne 0) { throw "ZIP creation failed. Log: $($Plan.Log)" }
        } finally { Pop-Location }
        $script:ProgressLastDraw = [datetime]::MinValue
        Show-BackupProgress 'Verifying ZIP'
        & $sevenZip t -tzip -bsp1 $partial 2>&1 | Tee-Object -FilePath $Plan.Log -Append | ForEach-Object {
            $percent = Get-ToolProgressPercent ([string]$_)
            if ($percent -ge 0) { Show-BackupProgress 'Verifying ZIP' $percent }
        }
        if ($LASTEXITCODE -ne 0) { throw "ZIP verification failed. Log: $($Plan.Log)" }
        [IO.File]::Move($partial, $Plan.Destination)
        Write-BackupEvent $Plan 'ZIP_CREATED' "Compression level: $Level. Archive verification passed. Original source retained."
        Show-BackupProgress 'ZIP complete' 100 Green
    } catch {
        Show-BackupProgress 'ZIP failed' -1 Red
        Write-BackupEvent $Plan 'ZIP_FAILED' $_.Exception.Message
        throw
    } finally {
        if (Test-Path -LiteralPath $partial) { Remove-Item -LiteralPath $partial -Force }
        Save-BackupSessionLog -WarningOnly
    }
}

function Show-BackupScreen([string]$Heading = 'File Backup v0.2.0-alpha') {
    Clear-Host
    $script:ProgressLastDraw = [datetime]::MinValue
    $script:ProgressRow = $null
    Write-BackupText '========================================================='
    Write-BackupText ''
    $text = $Heading.PadLeft([int]((57 + $Heading.Length) / 2))
    if ($Heading -match 'ERROR') { Write-BackupText $text -ForegroundColor Red }
    elseif ($Heading -match 'Complete|Renamed|Deleted|Saved|Successfully') { Write-BackupText $text -ForegroundColor Green }
    else { Write-BackupText $text }
    Write-BackupText ''
    Write-BackupText '========================================================='
    Write-BackupText ''
}

function Read-BackupSelectionKey {
    if ([Console]::IsInputRedirected) {
        $key = Read-Host 'UpArrow/DownArrow (blank selects)'
        if (-not $key) { return 'Enter' }
        return $key
    }
    return [Console]::ReadKey($true).Key
}

function Wait-BackupMenu {
    Write-BackupText 'Right arrow to return to menu'
    Read-BackupSelectionKey | Out-Null
}

function Read-BackupInput([string]$Prompt) {
    Write-BackupText $Prompt
    if ([Console]::IsInputRedirected) {
        $answer = Read-Host '>'
        if ($answer -eq 'LeftArrow') { return $null }
        return $answer
    }
    Write-BackupText '> ' -NoNewline
    $answer = [Text.StringBuilder]::new()
    while ($true) {
        $key = [Console]::ReadKey($true)
        if ($key.Key -eq 'LeftArrow') { [Console]::WriteLine(); return $null }
        if ($key.Key -eq 'Enter') { [Console]::WriteLine(); return $answer.ToString() }
        if ($key.Key -eq 'Backspace' -and $answer.Length) {
            $answer.Length--
            $column = [Console]::CursorLeft
            $row = [Console]::CursorTop
            if ($column -eq 0) { $column = 57; $row-- }
            [Console]::SetCursorPosition($column - 1, $row)
            [Console]::Write(' ')
            [Console]::SetCursorPosition($column - 1, $row)
        } elseif (-not [char]::IsControl($key.KeyChar)) {
            $answer.Append($key.KeyChar) | Out-Null
            if ([Console]::CursorLeft -ge 57) { [Console]::WriteLine() }
            [Console]::Write($key.KeyChar)
        }
    }
}

function Read-ExistingBackupName([switch]$AllowArchive) {
    $backups = @(if (Test-Path -LiteralPath $BackupRoot -PathType Container) {
        Get-ChildItem -LiteralPath $BackupRoot -Force |
            Where-Object { $_.Name -ne '.logs' -and ($_.PSIsContainer -or ($AllowArchive -and $_.Extension -ieq '.zip')) } |
            Sort-Object Name
    })
    if (-not $backups.Count) {
        Write-BackupText 'No backups available. Create a new backup first.'
        Read-BackupInput 'Press ENTER to return' | Out-Null
        return $null
    }
    $index = Read-BranchSelection @($backups.Name) '.backups'
    if ($null -ne $index) { return $backups[$index].Name }
    return $null
}

function Read-BranchSelection([string[]]$Items, [string]$Label, [switch]$MainMenu, [string]$Heading = 'Select Backup') {
    $selected = 0
    $column = [Math]::Min(24, $Label.Length + 3)
    $spine = ' ' * $column
    $indent = ' ' * ($column - 3)
    while ($true) {
        if ($MainMenu) { Show-BackupScreen } else { Show-BackupScreen $Heading }
        $headerRows = 1
        if ($Label.Length + 3 -gt 24) {
            Write-BackupText $Label
            Write-BackupText ($spine + '║')
            $headerRows = [int][Math]::Ceiling($Label.Length / 57.0) + 1
        } else { Write-BackupText "$Label ══╗" }
        Write-BackupText ($spine + '║')
        $visible = 4
        try { $visible = [Math]::Max(1, [int][Math]::Floor(([Console]::WindowHeight - 11 - $headerRows) / 2)) } catch { }
        $first = [Math]::Max(0, $selected - $visible + 1)
        $last = [Math]::Min($Items.Count - 1, $first + $visible - 1)
        if (-not $Items.Count) { Write-BackupText ($spine + '(Empty folder)') }
        for ($i = $first; $i -le $last; $i++) {
            $arrow = if ($i -eq $selected) { '>' } else { ' ' }
            $branch = if ($i -eq $Items.Count - 1) { '╚═══' } else { '╠═══' }
            $line = "$indent$arrow  $branch " + $Items[$i]
            if ($line.Length -gt 57) { $line = $line.Substring(0, 56) + '…' }
            Write-BackupText $line
            if ($i -lt $Items.Count - 1) { Write-BackupText ($spine + '║') }
        }
        Write-BackupText ''
        Write-BackupText 'Arrow keys to move'
        $key = [string](Read-BackupSelectionKey)
        switch ($key) {
            'UpArrow' { $selected = [Math]::Max(0, $selected - 1) }
            'DownArrow' { if ($Items.Count) { $selected = [Math]::Min($Items.Count - 1, $selected + 1) } }
            'Enter' { if ($Items.Count) { return $selected } }
            'RightArrow' { if ($Items.Count) { return $selected } }
            'LeftArrow' { if (-not $MainMenu) { return $null } }
        }
    }
}

function Read-MainMenuSelection {
    $items = @('Upload To a New/Existing Backup Folder', 'Create New ZIP Backup',
        'Compress Existing Backups to ZIP', 'Rename Backups', 'Delete Backups', 'View files', 'Edit config', 'Exit')
    $index = Read-BranchSelection $items 'Menu' -MainMenu
    return @('1', '2', '3', '4', '5', '6', '7', 'Q')[$index]
}

function Show-BackupBrowser([string]$Root = $PSScriptRoot) {
    $rootPath = [IO.Path]::GetFullPath($Root)
    if ($rootPath -ne [IO.Path]::GetPathRoot($rootPath)) { $rootPath = $rootPath.TrimEnd('\') }
    if (-not (Test-Path -LiteralPath $rootPath -PathType Container)) { throw 'Browser root does not exist.' }
    Assert-NoReparseAncestor $rootPath
    $rootName = Split-Path -Path $rootPath -Leaf
    if (-not $rootName) { $rootName = $rootPath.TrimEnd('\') }
    $current = $rootPath
    while ($true) {
        $entries = @(Get-ChildItem -LiteralPath $current -Force |
            Sort-Object @{ Expression = 'PSIsContainer'; Descending = $true }, Name)
        $names = @($entries | ForEach-Object { $_.Name })
        $relative = $current.TrimEnd('\').Substring($rootPath.TrimEnd('\').Length)
        $label = '...\' + $rootName + $relative + '\'
        $index = Read-BranchSelection $names $label -Heading 'View Files'
        if ($null -eq $index) {
            if ($current.Equals($rootPath, [StringComparison]::OrdinalIgnoreCase)) { return }
            $current = Split-Path -Path $current -Parent
            continue
        }
        $selected = Get-Item -LiteralPath $entries[$index].FullName -Force
        Assert-NoReparseAncestor $selected.FullName
        if (-not (Test-WithinPath $selected.FullName $rootPath)) { throw 'Selected path is outside the browser root.' }
        if ($selected.PSIsContainer) { $current = $selected.FullName }
        else {
            Start-Process -FilePath 'explorer.exe' -ArgumentList ('/select,"' + $selected.FullName + '"') -WindowStyle Normal
        }
    }
}

function Save-BackupConfigValue([string]$Name, $Value, [string]$ConfigPath = (Join-Path $PSScriptRoot 'config.json')) {
    if (-not (Get-BackupSettings $ConfigPath -Refresh).ContainsKey($Name)) { throw 'Unknown configuration setting.' }
    Assert-NoReparseAncestor $ConfigPath
    $config = if (Test-Path -LiteralPath $ConfigPath) { Get-Content -LiteralPath $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json }
              else { [pscustomobject](Get-BackupSettings $ConfigPath -Refresh) }
    $config | Add-Member -NotePropertyName $Name -NotePropertyValue $Value -Force
    $partial = $ConfigPath + '.partial'
    Assert-NoReparseAncestor $partial
    $created = $false
    try {
        $stream = [IO.File]::Open($partial, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
        $created = $true
        try {
            $bytes = [Text.Encoding]::UTF8.GetBytes(($config | ConvertTo-Json -Depth 10) + [Environment]::NewLine)
            $stream.Write($bytes, 0, $bytes.Length)
        } finally { $stream.Dispose() }
        Get-BackupSettings $partial | Out-Null
        Get-BackupDateStamp $partial | Out-Null
        $validated = Get-BackupSettings $partial
        foreach ($path in $validated.BackupRoot, $validated.LogDirectory) {
            Assert-NoReparseAncestor (Resolve-BackupSettingPath $path)
        }
        if (Test-Path -LiteralPath $ConfigPath) {
            [IO.File]::Replace($partial, $ConfigPath, [System.Management.Automation.Language.NullString]::Value)
        } else { [IO.File]::Move($partial, $ConfigPath) }
    } finally {
        if ($created -and (Test-Path -LiteralPath $partial)) { Remove-Item -LiteralPath $partial -Force }
    }
}

function Edit-BackupConfig([string]$ConfigPath = (Join-Path $PSScriptRoot 'config.json')) {
    $names = @('BackupRoot', 'LogDirectory', 'CompressionLevel', 'LogCompressionLevel', 'DateFormat',
        'ExcludeFiles', 'ExcludeFolders', 'OverwriteExistingFiles', 'CompletionDelaySeconds', 'CopyRetries', 'RetryDelaySeconds')
    while ($true) {
        $settings = Get-BackupSettings $ConfigPath -Refresh
        $labels = @($names | ForEach-Object { $_ + ': ' + (@($settings[$_]) -join ', ') })
        $index = Read-BranchSelection $labels 'Config' -Heading 'Edit Config'
        if ($null -eq $index) { return }
        $name = $names[$index]
        Show-BackupScreen 'Edit Config'
        Write-BackupText "$name current value: $(@($settings[$name]) -join ', ')"
        $prompt = 'New value'
        if ($name -in 'ExcludeFiles', 'ExcludeFolders') { $prompt = 'Patterns separated by commas (blank clears list)' }
        elseif ($name -eq 'OverwriteExistingFiles') { $prompt = 'New value: true or false' }
        elseif ($name -in 'CompressionLevel', 'LogCompressionLevel') { $prompt = 'New level: 0, 1, 3, 5, 6, 7, or 9' }
        $answer = Read-BackupInput $prompt
        if ($null -eq $answer) { continue }
        try {
            $value = $answer.Trim()
            if ($name -in 'ExcludeFiles', 'ExcludeFolders') {
                # ponytail: comma-separated patterns; edit JSON directly for patterns containing commas.
                $value = @(if ($value) { $value.Split(',') | ForEach-Object { $_.Trim() } })
            } elseif ($name -eq 'OverwriteExistingFiles') {
                $value = $false
                if (-not [bool]::TryParse($answer, [ref]$value)) { throw 'Enter true or false.' }
            } elseif ($name -in 'CompressionLevel', 'LogCompressionLevel', 'CompletionDelaySeconds', 'CopyRetries', 'RetryDelaySeconds') {
                $value = 0
                if (-not [int]::TryParse($answer, [ref]$value)) { throw 'Enter a whole number.' }
            }
            Save-BackupConfigValue $name $value $ConfigPath
            if ($script:SessionLog) {
                $session = [pscustomobject]@{ Source = 'FileBackup'; Destination = $ConfigPath; Log = $script:SessionLog }
                Write-BackupEvent $session 'CONFIG_UPDATED' "$name changed."
                Save-BackupSessionLog -WarningOnly
            }
            Show-BackupScreen 'Config Saved'
            Write-BackupText 'Restart the .bat to apply the saved settings.'
            Wait-BackupMenu
            return
        } catch {
            Show-BackupScreen 'ERROR DETECTED'
            Write-BackupText $_.Exception.Message -ForegroundColor Red
            Write-BackupText 'Right arrow to try again'
            Read-BackupSelectionKey | Out-Null
        }
    }
}

function Read-BackupRequest([string]$Selection) {
    $step = if ($Selection -eq '1') { 'Mode' } elseif ($Selection -eq '2') { 'Source' } else { 'Backup' }
    $history = [Collections.Generic.Stack[string]]::new()
    $existingName = ''; $newName = ''; $sourcePath = ''; $createNew = $false
    while ($true) {
        $back = $false
        Show-BackupScreen
        switch ($step) {
            'Mode' {
                $answer = Read-BackupInput 'Add to existing backup? (Y/N)'
                if ($null -eq $answer) { $back = $true }
                elseif (-not $answer) { return $null }
                elseif ($answer -ieq 'Y') { $createNew = $false; $next = 'Backup' }
                elseif ($answer -ieq 'N') { $createNew = $true; $existingName = ''; $next = 'Source' }
                else { throw 'Enter Y to add to an existing backup or N to create a new backup.' }
            }
            'Backup' {
                $existingName = Read-ExistingBackupName -AllowArchive:($Selection -in '4', '5')
                if ($null -eq $existingName) { $back = $true }
                elseif ($Selection -eq '1') { $next = 'Source' }
                elseif ($Selection -eq '4') { $next = 'Name' }
                else { $next = 'Confirm' }
            }
            'Source' {
                if ($Selection -eq '1') {
                    if ($createNew) { Write-BackupText 'A new folder backup uses the uploaded folder name.' }
                    else {
                        Write-BackupText "Backup folder: $existingName"
                        if ((Get-BackupSettings).OverwriteExistingFiles) { Write-BackupText 'Changed files with the same names will be overwritten; other files are kept.' }
                        else { Write-BackupText 'Existing files are protected; only missing files will be added.' }
                    }
                }
                Write-BackupText 'Drag and drop a file/folder, or type its full path.'
                $sourcePath = Read-BackupInput 'Source'
                if ($null -eq $sourcePath) { $back = $true }
                elseif (-not $sourcePath) { return $null }
                else {
                    $sourcePath = $sourcePath.Trim().Trim('"')
                    $next = 'Confirm'
                    if ($Selection -eq '1' -and $createNew) {
                        $existingName = ''
                        if (-not (Get-Item -LiteralPath $sourcePath -Force).PSIsContainer) { $next = 'Name' }
                    }
                }
            }
            'Name' {
                $prompt = if ($Selection -eq '4') { 'New backup name (include .zip for ZIP)' }
                          else { 'New backup folder name' }
                $answer = Read-BackupInput $prompt
                if ($null -eq $answer) { $back = $true }
                elseif (-not $answer) { return $null }
                else {
                    if ($Selection -eq '4') { $newName = $answer }
                    else { $existingName = $answer }
                    Get-NewBackupPath $BackupRoot $answer | Out-Null
                    $next = 'Confirm'
                }
            }
            'Confirm' {
                $level = $null
                switch ($Selection) {
                    '1' { $plan = New-BackupPlan $sourcePath $BackupRoot $existingName -CreateNew:$createNew }
                    '2' { $plan = New-ZipPlan $sourcePath $BackupRoot '' }
                    '3' { $plan = New-ZipPlan '' $BackupRoot $existingName }
                    '4' { $plan = New-BackupActionPlan $BackupRoot $existingName $newName }
                    '5' { $plan = New-BackupActionPlan $BackupRoot $existingName }
                }
                Write-BackupText "Source: $($plan.Source)"
                Write-BackupText "Backup: $($plan.Destination)"
                if ($Selection -in '2', '3') {
                    $level = Get-ZipCompressionLevel
                    Write-BackupText "ZIP compression level: $level"
                }
                if ($Selection -eq '5') {
                    Write-BackupText 'This permanently deletes the selected backup and its contents.'
                    $answer = Read-BackupInput 'Type DELETE to confirm (anything else cancels)'
                    $confirmed = $answer -ceq 'DELETE'
                } else {
                    $answer = Read-BackupInput 'Is this correct? (Y/N)'
                    $confirmed = $answer -ieq 'Y'
                }
                if ($null -eq $answer) { $back = $true }
                elseif ($confirmed) { return [pscustomobject]@{ Plan = $plan; Level = $level } }
                else {
                    Write-BackupEvent $plan ($plan.Operation + '_CANCELLED')
                    Save-BackupSessionLog -WarningOnly
                    return $null
                }
            }
        }
        if ($back) {
            if (-not $history.Count) { return $null }
            $step = $history.Pop()
        } else {
            $history.Push($step)
            $step = $next
        }
    }
}

# Dot sourcing exposes the backup functions to the regression check without opening the menu.
if ($MyInvocation.InvocationName -eq '.') { return }

$runExitCode = 0
try {
    $script:Settings = Get-BackupSettings
    Get-BackupDateStamp | Out-Null
    if (-not $PSBoundParameters.ContainsKey('BackupRoot')) { $BackupRoot = Resolve-BackupSettingPath $script:Settings.BackupRoot }
    if (-not $PSBoundParameters.ContainsKey('LogDirectory')) { $LogDirectory = Resolve-BackupSettingPath $script:Settings.LogDirectory }
    Start-BackupSession $BackupRoot $LogDirectory
    if ($NonInteractive) {
        $plan = New-BackupPlan $Source $BackupRoot $ExistingBackupName
        Invoke-Backup $plan
        Write-BackupText "$(Get-BackupCompletionMessage $plan): $($plan.Destination)" -ForegroundColor Green
        Write-BackupText "Log: $(Get-SavedLogPath $plan.Log)"
    } else {
    while ($true) {
    $selection = Read-MainMenuSelection
    if ($selection -ieq 'Q') { break }
    try {
        if ($selection -eq '6') { Show-BackupBrowser; continue }
        if ($selection -eq '7') { Edit-BackupConfig; continue }
        $request = Read-BackupRequest $selection
        if ($null -eq $request) { continue }
        $plan = $request.Plan
        if ($selection -in '4', '5') {
            Invoke-BackupAction $plan
            $heading = if ($selection -eq '4') { 'Backup Renamed' } else { 'Backup Deleted' }
            Show-BackupScreen $heading
            Write-BackupText "Backup: $($plan.Destination)"
            Write-BackupText "Log: $(Get-SavedLogPath $plan.Log)"
            Wait-BackupMenu
            continue
        }
        $level = $request.Level
        Write-BackupText 'Running backup...'
        if ($selection -eq '1') { Invoke-Backup $plan } else { Invoke-ZipBackup $plan $level }
        Start-Sleep -Seconds $script:Settings.CompletionDelaySeconds
        Show-BackupScreen (Get-BackupCompletionMessage $plan)
        Write-BackupText "Backup Folder: $($plan.Destination)"
        Write-BackupText "Log: $(Get-SavedLogPath $plan.Log)"
        Write-BackupText ''
        Wait-BackupMenu
    } catch {
        $errorMessage = $_.Exception.Message
        $session = [pscustomobject]@{ Source = 'FileBackup'; Destination = $BackupRoot; Log = $script:SessionLog }
        Write-BackupEvent $session 'ACTION_FAILED' $errorMessage
        Save-BackupSessionLog -WarningOnly
        Show-BackupScreen 'ERROR DETECTED'
        Write-BackupText "Action failed: $errorMessage" -ForegroundColor Red
        Write-BackupText ''
        $retry = Read-BackupInput 'Do you want to retry? (Y/N)'
        if ($null -eq $retry) { continue }
        if ($retry -ine 'Y') { break }
    }
}
    }
} catch {
    $runExitCode = 1
    if ($script:SessionLog) {
        $session = [pscustomobject]@{ Source = 'FileBackup'; Destination = $BackupRoot; Log = $script:SessionLog }
        Write-BackupEvent $session 'RUN_FAILED' $_.Exception.Message
    }
    Write-BackupText $_.Exception.Message -ForegroundColor Red
} finally {
    try { Save-BackupSessionLog -Final } catch {
        $runExitCode = 1
        Write-BackupText "Log compression failed. Uncompressed log retained. $($_.Exception.Message)" -ForegroundColor Red
    }
}
if ($NonInteractive) { exit $runExitCode }
### FILEBACKUP:END manager.ps1

### FILEBACKUP:BEGIN config.json
{
  "CompressionLevel": 6,
  "DateFormat": "{year}-{month}-{day}_{hour}-{minute}-{second}",
  "BackupRoot": ".backups",
  "LogDirectory": ".logs",
  "LogCompressionLevel": 6,
  "ExcludeFiles": [],
  "ExcludeFolders": [],
  "OverwriteExistingFiles": true,
  "CompletionDelaySeconds": 2,
  "CopyRetries": 3,
  "RetryDelaySeconds": 5
}
### FILEBACKUP:END config.json
