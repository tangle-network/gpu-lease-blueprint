// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Script, console2} from "forge-std/Script.sol";
import "tnt-core/libraries/Types.sol";
import "../src/GpuLeaseBlueprint.sol";
import "../src/GpuLeaseVault.sol";

/// @notice Minimal interface for the Tangle master manager (blueprint registration).
interface ITangle {
    function createBlueprint(Types.BlueprintDefinition calldata def) external returns (uint64);
}

/// @title RegisterGpuLeaseBlueprint
/// @notice Deploys the vault (money layer) + BSM (routing layer) and registers
///         the blueprint on Tangle. Job order MUST match the Rust constants:
///         LEASE=0, RELEASE=1, EXTEND=2, REAP=3.
/// @dev Run: forge script contracts/script/RegisterGpuLeaseBlueprint.s.sol --rpc-url $RPC_URL --broadcast --slow
contract RegisterGpuLeaseBlueprint is Script {
    // Anvil well-known deployer key (default when no PRIVATE_KEY env is set)
    uint256 constant DEFAULT_DEPLOYER_KEY = 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80;

    // Tangle protocol addresses on a LocalTestnet anvil snapshot.
    address constant DEFAULT_TANGLE = 0xDc64a140Aa3E981100a9becA4E685f962f0cF6C9;

    function run() external {
        uint256 deployerKey = vm.envOr("PRIVATE_KEY", DEFAULT_DEPLOYER_KEY);
        address tangleAddr = vm.envOr("TANGLE_CORE", DEFAULT_TANGLE);

        ITangle tangle = ITangle(tangleAddr);

        vm.startBroadcast(deployerKey);

        GpuLeaseVault vault = new GpuLeaseVault();
        GpuLeaseBlueprint bsm = new GpuLeaseBlueprint(address(vault));

        uint64 blueprintId = tangle.createBlueprint(_buildDefinition(address(bsm)));

        vm.stopBroadcast();

        console2.log("GPU_LEASE_VAULT=%s", address(vault));
        console2.log("GPU_LEASE_BSM=%s", address(bsm));
        console2.log("GPU_LEASE_BLUEPRINT_ID=%s", blueprintId);
    }

    function _buildJobs() internal pure returns (Types.JobDefinition[] memory jobs) {
        jobs = new Types.JobDefinition[](4);
        // Indices are positional — MUST match JOB_LEASE..JOB_REAP (0..3) and the Rust router.
        jobs[0] = Types.JobDefinition("lease", "Create an escrowed GPU lease (allocates a device)", "", "", "");
        jobs[1] = Types.JobDefinition("release", "Voluntarily release a lease (pro-rata refund on the vault)", "", "", "");
        jobs[2] = Types.JobDefinition("extend", "Extend a lease session (escrow topped up on the vault)", "", "", "");
        jobs[3] = Types.JobDefinition("reap", "Permissionless post-expiry teardown", "", "", "");
    }

    function _buildDefinition(address manager) internal pure returns (Types.BlueprintDefinition memory def) {
        def.metadataUri = "https://github.com/tangle-network/gpu-lease-blueprint";
        def.metadataHash = keccak256(bytes(def.metadataUri));
        def.manager = manager;
        def.masterManagerRevision = 0;
        def.hasConfig = true;

        def.config = Types.BlueprintConfig({
            membership: Types.MembershipModel.Dynamic,
            pricing: Types.PricingModel.EventDriven,
            minOperators: 1,
            maxOperators: 100,
            subscriptionRate: 0,
            subscriptionInterval: 0,
            eventRate: 1e15 // 0.001 TNT base rate
        });

        def.metadata = Types.BlueprintMetadata({
            name: "GPU Lease Blueprint",
            description: "Escrowed GPU leases: parking-meter economics, RFQ pricing, TEE-bound quotes",
            author: "Tangle",
            category: "Compute",
            codeRepository: "https://github.com/tangle-network/gpu-lease-blueprint",
            logo: "",
            website: "https://tangle.network",
            license: "MIT OR Apache-2.0",
            profilingData: ""
        });

        def.jobs = _buildJobs();
        def.registrationSchema = "";
        def.requestSchema = "";
        def.sources = new Types.BlueprintSource[](0);
    }
}
