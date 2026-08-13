#!/bin/bash

CMAKE_INSTALLER_URL="https://github.com/Kitware/CMake/releases/download/v4.4.2/cmake-4.4.2-linux-x86_64.sh"
CMAKE_INSTALL_DIR=/usr/local/cmake
CMAKE_INSTALLER_PATH=/tmp/cmake_install.sh

apt udpate -y && \
apt install -y --no-install-recommends \
  curl \
  build-essential \
  ninja-build \
  vim \
  llvm \
  clang \
  clangd 

mkdir -p $CMAKE_INSTALL_DIR                                   \
  && curl -o $CMAKE_INSTALLER_PATH $CMAKE_INSTALLER_URL -fsSL \
  && chmod +x $CMAKE_INSTALLER_PATH                           \
  && $CMAKE_INSTALLER_PATH --skip-license --prefix=$CMAKE_INSTALL_DIR
