// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Script, console2} from "forge-std/Script.sol";
import {GpuLeaseVault} from "../src/GpuLeaseVault.sol";

/// @notice Deploy the GPU lease vault (the blueprint's money layer).
/// @dev Select the signing account with Forge's --account or hardware wallet options.
contract DeployGpuLeaseVault is Script {
    function run() external returns (GpuLeaseVault vault) {
        vm.startBroadcast();
        vault = new GpuLeaseVault();
        vm.stopBroadcast();
        console2.log("GPU_LEASE_VAULT=%s", address(vault));
    }
}
