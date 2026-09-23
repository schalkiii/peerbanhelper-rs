Set-Location 'd:\workspace\peerbanhelper-rs'
cargo build --release -p pbh *> target\release_build.log
Write-Output 'BUILD-DONE' *> target\release_build.log
