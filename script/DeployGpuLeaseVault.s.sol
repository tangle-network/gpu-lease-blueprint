// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Script, console2} from "forge-std/Script.sol";
import {GpuLeaseVault} from "../src/GpuLeaseVault.sol";

/// @notice Deploy the GPU lease vault (the blueprint's money layer).
/// @dev Run: forge script script/DeployGpuLeaseVault.s.sol --rpc-url $RPC_URL --broadcast
contract DeployGpuLeaseVault is Script {
    uint256 constant DEFAULT_DEPLOYER_KEY = 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80; // anvil #0

    function run() external returns (GpuLeaseVault vault) {
        uint256 deployerKey = vm.envOr("PRIVATE_KEY", DEFAULT_DEPLOYER_KEY);
        vm.startBroadcast(deployerKey);
        vault = new GpuLeaseVault();
        vm.stopBroadcast();
        console2.log("GPU_LEASE_VAULT=%s", address(vault));
    }
}
