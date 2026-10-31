@echo off
setlocal EnableDelayedExpansion
set "ROOT=%~dp0.."

if "%~1"=="" (
  echo create-sim: missing topology script 1>&2
  echo Usage: %~nx0 ^<script.py^> [--server URL] [--open] 1>&2
  exit /b 2
)

set "SCRIPT="
set "SKIP="
for %%A in (%*) do (
  if defined SKIP (
    set "SKIP="
  ) else if /I "%%~A"=="--server" (
    set "SKIP=1"
  ) else if /I "%%~A"=="--open" (
    rem flag
  ) else (
    set "ARG=%%~A"
    if "!ARG:~0,1!"=="-" (
      echo create-sim: unknown argument %%~A 1>&2
      exit /b 2
    )
    if /I "%%~xA"==".py" (
      if defined SCRIPT (
        echo create-sim: unexpected extra argument: %%~A 1>&2
        exit /b 2
      )
      set "SCRIPT=%%~A"
    ) else (
      echo create-sim: topology script must be a .py path: %%~A 1>&2
      exit /b 2
    )
  )
)

if not defined SCRIPT (
  echo create-sim: missing topology script 1>&2
  echo Usage: %~nx0 ^<script.py^> [--server URL] [--open] 1>&2
  exit /b 2
)
if not exist "%SCRIPT%" (
  echo create-sim: script not found: %SCRIPT% 1>&2
  exit /b 2
)

set "PYTHONPATH=%ROOT%\python;%PYTHONPATH%"
python -m topology_dsl create %*
