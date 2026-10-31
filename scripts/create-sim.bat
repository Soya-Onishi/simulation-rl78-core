@echo off
setlocal
set "ROOT=%~dp0.."
set "PYTHONPATH=%ROOT%\python;%PYTHONPATH%"
python -m topology_dsl create %*
