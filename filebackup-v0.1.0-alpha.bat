@echo off
setlocal enabledelayedexpansion
title File Backup - by godblessmerica
mode con: cols=58 lines=30
cls

set "MAIN_BACKUP_DIR=%~dp0Backups"
if not exist "!MAIN_BACKUP_DIR!" (
    mkdir "!MAIN_BACKUP_DIR!"
)

:START
cls
color

set "SOURCE_DIR="
set "SUB_FOLDER="
set "FINAL_DEST="
set "FOLDER="
set "FOLDER_NAME="

echo =========================================================
echo.
echo                      File Backup    
echo.
echo =========================================================
echo.
echo Please drag and drop the folder you want to back up here, 
echo or type the full path manually, then press ENTER:
echo.

set /p "SOURCE_DIR=> "
if not defined SOURCE_DIR goto START

set "SOURCE_DIR=!SOURCE_DIR:"=!"
if "!SOURCE_DIR:~-1!"=="\" set "SOURCE_DIR=!SOURCE_DIR:~0,-1!"

if exist "!SOURCE_DIR!\" (
    set "FOLDER=TRUE"
    set "FINAL_DEST=!MAIN_BACKUP_DIR!"
    call :GetFolderName "!SOURCE_DIR!"
) else if exist "!SOURCE_DIR!" (
    set "FOLDER=FALSE"
    
    :CREATE_FOLDER
    cls 
    echo =========================================================
    echo.
    echo                      Create A Folder
    echo.
    echo =========================================================
    echo.
    echo Please type a name for the folder to keep this file
    echo.
    set /p "SUB_FOLDER=> "
    if not defined SUB_FOLDER goto CREATE_FOLDER
    set "FINAL_DEST=!MAIN_BACKUP_DIR!\!SUB_FOLDER!"    
) else (
    goto ERROR
)

cls
echo =========================================================
echo.
echo                      Starting Backup 
echo.
echo =========================================================
echo.
echo Source Folder: !SOURCE_DIR!
echo Backup Folder: !FINAL_DEST!
echo.
echo Is this correct? (Y/N)
choice /c yn /n /m "> "
if %errorlevel% equ 2 goto START

echo.
echo Running backup...
echo.

if "!FOLDER!"=="TRUE" (
    robocopy "!SOURCE_DIR!" "!FINAL_DEST!\!FOLDER_NAME!" /E /Z /R:3 /W:5
    if not errorlevel 8 (
        goto SUCCESS_SCREEN
    ) else (
        goto ERROR
    )
) else (
    if not exist "!FINAL_DEST!" mkdir "!FINAL_DEST!"
    copy /Y "!SOURCE_DIR!" "!FINAL_DEST!\" > nul
    if not errorlevel 1 (
        goto SUCCESS_SCREEN
    ) else (
        goto ERROR
    )
)

:SUCCESS_SCREEN
cls
echo =========================================================
echo.
echo                      Backup Complete
echo.
echo =========================================================
echo.
echo Do you want to back up more files? (Y/N)
choice /c yn /n /m "> "
if %errorlevel% equ 1 goto START
exit

:ERROR
cls
echo =========================================================
echo.
echo                      ERROR DETECTED
echo.
echo =========================================================
echo.
echo The backup process has FAILED. 
echo.
echo Possible reasons:
echo   - The source folder path you entered does not exist
echo     or was misspelled.
echo   - The backup drive/location is full or disconnected.
echo   - You do not have permission to copy these files.
echo.
echo =========================================================
echo Please check your folder path and try again.
echo.
echo Do you want to retry? (Y/N)
choice /c yn /n /m "> "
if %errorlevel% equ 1 goto START
exit

:GetFolderName
set "FOLDER_NAME=%~nx1"
goto :eof
