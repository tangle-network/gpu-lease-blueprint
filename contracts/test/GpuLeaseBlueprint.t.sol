// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {GpuLeaseBlueprint} from "../src/GpuLeaseBlueprint.sol";
import {GpuLeaseVault} from "../src/GpuLeaseVault.sol";

/// @title GpuLeaseBlueprint tnt-core integration test — the BSM wiring that
///        tnt-core exercises: onJobCall → operator → onJobResult, with the
///        result bound against the REAL vault money state (I5 edge).
contract GpuLeaseBlueprintTest is Test {
    GpuLeaseVault internal vault;
    GpuLeaseBlueprint internal bsm;

    address internal tangleCore = address(0x7A); // the master manager (onlyFromTangle)
    address internal blueprintOwner = address(0xBB);
    uint64 internal testBlueprintId = 42;
    uint64 internal serviceId = 7;

    address internal operator = makeAddr("operator");
    address internal lessee = makeAddr("lessee");

    uint128 internal constant PRICE = 100;
    uint64 internal constant DURATION = 100;

    bytes32 internal intentHash;
    bytes32 internal leaseId;

    // Hoisted job ids: reading bsm.JOB_* inside a vm.prank'd call list would
    // consume the prank (the constant read is itself a call to the contract).
    uint8 internal jobLease;
    uint8 internal jobRelease;

    function setUp() public {
        vault = new GpuLeaseVault();
        bsm = new GpuLeaseBlueprint(address(vault), address(0));
        jobLease = bsm.JOB_LEASE();
        jobRelease = bsm.JOB_RELEASE();
        // tnt-core initializes the BSM at blueprint creation.
        vm.prank(tangleCore);
        bsm.onBlueprintCreated(testBlueprintId, blueprintOwner, tangleCore);

        intentHash = keccak256("gpu-lease-intent|v1|h100|100|0|us-east");
    }

    /// The job request, bound to the vault lease the buyer already escrowed.
    function _request(bytes32 vaultLeaseId) internal view returns (bytes memory) {
        GpuLeaseBlueprint.GpuLeaseRequest memory request = GpuLeaseBlueprint.GpuLeaseRequest({
            intentVersion: 1,
            intentHash: intentHash,
            pricePerSecond: PRICE,
            durationSeconds: DURATION,
            confidentiality: 0,
            gpuClass: "h100",
            region: "us-east",
            lessee: lessee,
            leaseId: vaultLeaseId,
            sandboxId: bytes32(0),
            sandboxTeeType: 0,
            deviceCount: 1
        });
        return abi.encode(request);
    }

    /// Full happy path: lessee escrows on the vault → tnt-core relays the job →
    /// operator returns the leaseId → BSM binds it after cross-checking the vault.
    function test_LeaseJob_BindsAgainstRealVaultLease() public {
        // 1. Buyer creates the escrowed lease on the vault (money layer).
        vm.deal(lessee, 1 ether);
        vm.prank(lessee);
        leaseId = vault.create{value: uint256(PRICE) * DURATION}(
            operator, PRICE, DURATION, 1, intentHash, 0, "{\"v\":1,\"transport\":\"local-attach\"}"
        );

        // 2. tnt-core relays the job call (caches inputs, bound to the vault leaseId).
        bytes memory leaseRequest = _request(leaseId);
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobLease, 1, leaseRequest);

        // 3. Operator result: the leaseId the vault just created.
        GpuLeaseBlueprint.GpuLeaseOutput memory output = GpuLeaseBlueprint.GpuLeaseOutput({
            leaseId: leaseId,
            endpoint: "{\"v\":1,\"transport\":\"local-attach\",\"device\":0}",
            schemaVersion: 1
        });
        vm.prank(tangleCore);
        vm.expectEmit(true, true, true, true);
        emit GpuLeaseBlueprint.LeaseBound(leaseId, operator, serviceId, 1);
        bsm.onJobResult(serviceId, jobLease, 1, operator, keccak256(_request(leaseId)), abi.encode(output));

        assertEq(bsm.leaseOperatorOf(leaseId), operator, "leaseId bound to operator");
    }

    /// The trust edge: an operator claiming a leaseId that does NOT exist on the
    /// vault must fail closed.
    function test_LeaseJob_RevertWhenVaultLeaseDoesNotExist() public {
        bytes32 deadId = bytes32(uint256(0xdead));
        bytes memory leaseRequest = _request(deadId);
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobLease, 1, leaseRequest);
        GpuLeaseBlueprint.GpuLeaseOutput memory output = GpuLeaseBlueprint.GpuLeaseOutput(deadId, "{}", 1);
        vm.prank(tangleCore);
        vm.expectPartialRevert(GpuLeaseBlueprint.VaultLeaseNotLive.selector);
        bsm.onJobResult(serviceId, jobLease, 1, operator, keccak256(leaseRequest), abi.encode(output));
    }

    /// Intent swap: request intent != vault lease intent → revert (I5).
    function test_LeaseJob_RevertWhenIntentMismatch() public {
        vm.deal(lessee, 1 ether);
        vm.prank(lessee);
        // Vault lease created with a DIFFERENT intent than the job request carries.
        bytes32 otherIntent = keccak256("gpu-lease-intent|v1|b200|100|0|eu-west");
        leaseId = vault.create{value: uint256(PRICE) * DURATION}(
            operator, PRICE, DURATION, 1, otherIntent, 0, "{}"
        );
        bytes memory leaseRequest = _request(leaseId);
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobLease, 1, leaseRequest);
        GpuLeaseBlueprint.GpuLeaseOutput memory output = GpuLeaseBlueprint.GpuLeaseOutput(leaseId, "{}", 1);
        vm.prank(tangleCore);
        vm.expectPartialRevert(GpuLeaseBlueprint.IntentMismatch.selector);
        bsm.onJobResult(serviceId, jobLease, 1, operator, keccak256(_request(leaseId)), abi.encode(output));
    }

    /// Wrong operator: vault lease names operator A, result submitted by B.
    function test_LeaseJob_RevertWhenOperatorMismatch() public {
        vm.deal(lessee, 1 ether);
        vm.prank(lessee);
        leaseId = vault.create{value: uint256(PRICE) * DURATION}(operator, PRICE, DURATION, 1, intentHash, 0, "{}");
        bytes memory leaseRequest = _request(leaseId);
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobLease, 1, leaseRequest);
        GpuLeaseBlueprint.GpuLeaseOutput memory output = GpuLeaseBlueprint.GpuLeaseOutput(leaseId, "{}", 1);
        address impostor = makeAddr("impostor");
        vm.prank(tangleCore);
        vm.expectPartialRevert(GpuLeaseBlueprint.OperatorMismatch.selector);
        bsm.onJobResult(serviceId, jobLease, 1, impostor, keccak256(_request(leaseId)), abi.encode(output));
    }

    /// Lessee binding: vault lease belongs to lessee A, request names B.
    function test_LeaseJob_RevertWhenLesseeMismatch() public {
        vm.deal(lessee, 1 ether);
        vm.prank(lessee);
        leaseId = vault.create{value: uint256(PRICE) * DURATION}(operator, PRICE, DURATION, 1, intentHash, 0, "{}");
        bytes memory leaseRequest = _request(leaseId);
        // Request claims a different lessee.
        GpuLeaseBlueprint.GpuLeaseRequest memory tampered = abi.decode(leaseRequest, (GpuLeaseBlueprint.GpuLeaseRequest));
        tampered.lessee = makeAddr("attacker");
        // Re-hash the tampered intent so only the lessee differs.
        bytes memory tamperedInputs = abi.encode(tampered);
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobLease, 1, tamperedInputs);
        GpuLeaseBlueprint.GpuLeaseOutput memory output = GpuLeaseBlueprint.GpuLeaseOutput(leaseId, "{}", 1);
        vm.prank(tangleCore);
        vm.expectPartialRevert(GpuLeaseBlueprint.LesseeMismatch.selector);
        bsm.onJobResult(serviceId, jobLease, 1, operator, keccak256(tamperedInputs), abi.encode(output));
    }

    /// Unknown schema version fails closed (SPEC §3).
    function test_LeaseJob_RevertWhenUnknownSchemaVersion() public {
        vm.deal(lessee, 1 ether);
        vm.prank(lessee);
        leaseId = vault.create{value: uint256(PRICE) * DURATION}(operator, PRICE, DURATION, 1, intentHash, 0, "{}");
        bytes memory leaseRequest = _request(leaseId);
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobLease, 1, leaseRequest);
        GpuLeaseBlueprint.GpuLeaseOutput memory output = GpuLeaseBlueprint.GpuLeaseOutput(leaseId, "{}", 99);
        vm.prank(tangleCore);
        vm.expectPartialRevert(GpuLeaseBlueprint.UnknownSchemaVersion.selector);
        bsm.onJobResult(serviceId, jobLease, 1, operator, keccak256(_request(leaseId)), abi.encode(output));
    }

    /// RELEASE routes to the bound operator; anyone else reverts.
    function test_ReleaseJob_RoutesToBoundOperator() public {
        _createAndBindLease();
        bytes memory releaseInputs = abi.encode(GpuLeaseBlueprint.GpuLeaseIdRequest(leaseId));

        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobRelease, 2, releaseInputs);
        vm.prank(tangleCore);
        vm.expectEmit(true, true, true, true);
        emit GpuLeaseBlueprint.LeaseSettled(leaseId, operator, 1);
        bsm.onJobResult(serviceId, jobRelease, 2, operator, keccak256(releaseInputs), "");

        assertEq(bsm.leaseStateOf(leaseId), 1, "released state mirror");

        // Impostor cannot release.
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobRelease, 3, releaseInputs);
        address impostor = makeAddr("impostor");
        vm.prank(tangleCore);
        vm.expectPartialRevert(GpuLeaseBlueprint.OperatorMismatch.selector);
        bsm.onJobResult(serviceId, jobRelease, 3, impostor, keccak256(releaseInputs), "");
    }

    /// Unknown leaseId on RELEASE fails closed.
    function test_ReleaseJob_RevertWhenLeaseNotFound() public {
        bytes memory releaseInputs = abi.encode(GpuLeaseBlueprint.GpuLeaseIdRequest(bytes32(uint256(0xbeef))));
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobRelease, 1, releaseInputs);
        vm.prank(tangleCore);
        vm.expectPartialRevert(GpuLeaseBlueprint.LeaseNotFound.selector);
        bsm.onJobResult(serviceId, jobRelease, 1, operator, keccak256(releaseInputs), "");
    }

    /// Unknown job id reverts.
    function test_OnJobCall_RevertUnknownJob() public {
        vm.prank(tangleCore);
        vm.expectRevert(abi.encodeWithSelector(GpuLeaseBlueprint.UnknownJobId.selector, 9));
        bsm.onJobCall(serviceId, 9, 1, "");
    }

    /// Only tnt-core may call the hooks.
    function test_OnlyFromTangle() public {
        address rando = address(0x9999);
        vm.prank(rando);
        vm.expectPartialRevert(bytes4(0x065babc6));
        bsm.onJobCall(serviceId, 0, 1, "");
    }

    /// Full money + routing E2E at the contract level: escrow → bind →
    /// vault release (pro-rata refund) → BSM settle mirror.
    function test_FullLifecycle_VaultMoneyAndBsmRouting() public {
        _createAndBindLease();

        // Lessee releases on the vault — exact pro-rata (40s elapsed).
        vm.warp(block.timestamp + 40);
        uint256 lesseeBefore = lessee.balance;
        vm.prank(lessee);
        vault.release(leaseId);
        assertEq(lessee.balance - lesseeBefore, PRICE * 60, "I3 exact refund alongside BSM routing");

        // BSM mirrors the settlement when the operator's release result lands.
        bytes memory releaseInputs = abi.encode(GpuLeaseBlueprint.GpuLeaseIdRequest(leaseId));
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobRelease, 2, releaseInputs);
        vm.prank(tangleCore);
        bsm.onJobResult(serviceId, jobRelease, 2, operator, keccak256(releaseInputs), "");
        assertEq(bsm.leaseStateOf(leaseId), 1);
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    function _createAndBindLease() internal {
        vm.deal(lessee, 1 ether);
        vm.prank(lessee);
        leaseId = vault.create{value: uint256(PRICE) * DURATION}(operator, PRICE, DURATION, 1, intentHash, 0, "{}");
        bytes memory leaseRequest = _request(leaseId);
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobLease, 1, leaseRequest);
        GpuLeaseBlueprint.GpuLeaseOutput memory output = GpuLeaseBlueprint.GpuLeaseOutput(leaseId, "{}", 1);
        vm.prank(tangleCore);
        bsm.onJobResult(serviceId, jobLease, 1, operator, keccak256(_request(leaseId)), abi.encode(output));
    }

    /// Quantity binding: the vault lease paid for 2 devices but the job
    /// request claims 1 — the operator would under-deliver. Fail closed.
    function test_LeaseJob_RevertWhenDeviceCountMismatch() public {
        vm.deal(lessee, 1 ether);
        vm.prank(lessee);
        bytes32 twoDeviceLease = vault.create{value: uint256(PRICE) * DURATION * 2}(
            operator, PRICE, DURATION, 2, intentHash, 0, "{}"
        );
        bytes memory leaseRequest = _request(twoDeviceLease); // deviceCount: 1
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobLease, 1, leaseRequest);
        GpuLeaseBlueprint.GpuLeaseOutput memory output =
            GpuLeaseBlueprint.GpuLeaseOutput(twoDeviceLease, "{}", 1);
        vm.prank(tangleCore);
        vm.expectRevert(
            abi.encodeWithSelector(GpuLeaseBlueprint.DeviceCountMismatch.selector, 1, 2)
        );
        bsm.onJobResult(
            serviceId, jobLease, 1, operator, keccak256(_request(twoDeviceLease)), abi.encode(output)
        );
    }

    /// The multi-GPU happy path: a 2-device vault lease binds when the job
    /// request carries the same count.
    function test_LeaseJob_MultiDevice_BindsMatchingCount() public {
        vm.deal(lessee, 2 ether);
        vm.prank(lessee);
        bytes32 id = vault.create{value: uint256(PRICE) * DURATION * 2}(
            operator, PRICE, DURATION, 2, intentHash, 0, "{}"
        );
        GpuLeaseBlueprint.GpuLeaseRequest memory request = abi.decode(
            _request(id), (GpuLeaseBlueprint.GpuLeaseRequest)
        );
        request.deviceCount = 2;
        bytes memory leaseRequest = abi.encode(request);
        vm.prank(tangleCore);
        bsm.onJobCall(serviceId, jobLease, 1, leaseRequest);
        GpuLeaseBlueprint.GpuLeaseOutput memory output = GpuLeaseBlueprint.GpuLeaseOutput(id, "{}", 1);
        vm.prank(tangleCore);
        bsm.onJobResult(serviceId, jobLease, 1, operator, keccak256(leaseRequest), abi.encode(output));
        assertEq(bsm.leaseOperatorOf(id), operator, "2-device lease bound");
    }
}
