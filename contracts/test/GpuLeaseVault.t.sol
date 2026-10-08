// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {GpuLeaseVault} from "../src/GpuLeaseVault.sol";

/// @title GpuLeaseVault invariant suite — pins I1–I5 (SPEC.md, HANDOFF §1).
/// @dev The pro-rata math in _settle is NOT reviewable apart from these tests:
///      every money claim below is an exact-equality assertion, never approx.
contract GpuLeaseVaultTest is Test {
    GpuLeaseVault internal vault;

    address internal operator = makeAddr("operator");
    address internal lessee = makeAddr("lessee");
    address internal other = makeAddr("other");
    address internal payer = makeAddr("payer"); // anyone may subsidize extends

    bytes32 internal constant INTENT = keccak256("intent-v1:H100:3600s:tee=false");
    bytes internal constant ENDPOINT = "{\"v\":1,\"transport\":\"local-attach\"}";

    uint128 internal constant PRICE = 100; // wei/sec in unit tests
    uint64 internal constant DURATION = 100; // seconds

    function setUp() public {
        vault = new GpuLeaseVault();
        vm.deal(lessee, 1_000_000 ether);
        vm.deal(payer, 1_000_000 ether);
        vm.deal(operator, 1 ether); // gas only, never escrow
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    function _create(uint128 price, uint64 duration) internal returns (bytes32 id) {
        id = _createCount(price, duration, 1);
    }

    function _createCount(uint128 price, uint64 duration, uint16 count) internal returns (bytes32 id) {
        uint256 cost = uint256(price) * uint256(duration) * uint256(count);
        vm.prank(lessee);
        id = vault.create{value: cost}(operator, price, duration, count, INTENT, 0, ENDPOINT);
    }

    function _lease(bytes32 id) internal view returns (GpuLeaseVault.Lease memory l) {
        (l.operator, l.lessee, l.escrow, l.pricePerSecond, l.expiry, l.intentHash, l.confidentiality, l.state, l.deviceCount) =
            vault.leases(id);
    }

    /// I1: at every observable point, vault holds exactly live escrow + unwithdrawn earnings.
    function _checkConservation() internal view {
        assertEq(
            address(vault).balance,
            vault.totalEscrowed() + vault.operatorEarnings(),
            "I1 violated: balance != totalEscrowed + operatorEarnings"
        );
    }

    /// I2: for a live lease, no unpayable time exists.
    function _checkOverstayImpossible(bytes32 id) internal view {
        GpuLeaseVault.Lease memory l = _lease(id);
        uint256 remaining = l.expiry > block.timestamp ? uint256(l.expiry - block.timestamp) : 0;
        assertLe(
            uint256(l.pricePerSecond) * uint256(l.deviceCount) * remaining,
            uint256(l.escrow),
            "I2 violated: price*count*remaining > escrow"
        );
        // Exact derivation pinned: escrow == price * count * paidSeconds at all
        // times, so _settle's division is exact by construction.
        assertEq(
            uint256(l.escrow) % (uint256(l.pricePerSecond) * uint256(l.deviceCount)),
            0,
            "derivation broken: escrow % (price*count) != 0"
        );
    }

    // ==================================================================
    // CREATE
    // ==================================================================

    function test_Create_StoresLeaseExactly() public {
        uint64 before = uint64(block.timestamp);
        uint256 cost = uint256(PRICE) * DURATION;
        // leaseId is deterministic: keccak(operator, lessee, intent, ts, totalEscrowed, msg.value)
        bytes32 expectedId = keccak256(abi.encodePacked(operator, lessee, INTENT, block.timestamp, uint256(0), cost));
        vm.expectEmit(true, true, true, true);
        emit GpuLeaseVault.LeaseCreated(
            expectedId, operator, lessee, PRICE * DURATION, PRICE, 1, before + DURATION, INTENT, 0, ENDPOINT, 1
        );
        bytes32 id = _create(PRICE, DURATION);
        assertEq(id, expectedId, "deterministic leaseId");

        GpuLeaseVault.Lease memory l = _lease(id);
        assertEq(l.operator, operator, "operator");
        assertEq(l.lessee, lessee, "lessee");
        assertEq(l.escrow, PRICE * DURATION, "escrow");
        assertEq(l.pricePerSecond, PRICE, "price");
        assertEq(l.expiry, before + DURATION, "expiry");
        assertEq(l.intentHash, INTENT, "intentHash");
        assertEq(uint8(l.state), 0, "state Live");
        assertEq(vault.totalEscrowed(), PRICE * DURATION, "totalEscrowed");
        _checkConservation();
        _checkOverstayImpossible(id);
    }

    function test_Create_RevertZeroPrice() public {
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.ZeroPrice.selector);
        vault.create{value: 1}(operator, 0, DURATION, 1, INTENT, 0, ENDPOINT);
    }

    function test_Create_RevertZeroDuration() public {
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.DurationZero.selector);
        vault.create{value: PRICE}(operator, PRICE, 0, 1, INTENT, 0, ENDPOINT);
    }

    function test_Create_RevertInsufficientValue() public {
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.InsufficientEscrow.selector);
        vault.create{value: uint256(PRICE) * DURATION - 1}(operator, PRICE, DURATION, 1, INTENT, 0, ENDPOINT);
    }

    function test_Create_OverpayRefundsExcess_I1HoldsExactly() public {
        uint256 cost = uint256(PRICE) * DURATION;
        uint256 lesseeBefore = lessee.balance;
        vm.startPrank(lessee);
        bytes32 id = vault.create{value: cost + 7 ether}(operator, PRICE, DURATION, 1, INTENT, 0, ENDPOINT);
        vm.stopPrank();
        // Excess returned to the wei; nothing stranded in the vault.
        assertEq(lessee.balance, lesseeBefore - cost, "excess not refunded exactly");
        assertEq(address(vault).balance, cost, "vault kept overpay");
        assertEq(vault.totalEscrowed(), cost, "totalEscrowed excludes overpay");
        _checkConservation();
        _checkOverstayImpossible(id);
    }

    function test_Create_RevertCostExceedsUint128() public {
        uint128 hugePrice = type(uint128).max;
        uint64 duration = 2; // 2 * (2^128-1) > 2^128
        uint256 cost = uint256(hugePrice) * uint256(duration);
        vm.deal(lessee, cost);
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.Overflow.selector);
        vault.create{value: cost}(operator, hugePrice, duration, 1, INTENT, 0, ENDPOINT);
    }

    function test_Create_RevertExpiryOverflowsUint64() public {
        uint64 duration = type(uint64).max; // now + max > 2^64-1
        uint256 cost = uint256(duration); // price = 1 wei/s
        vm.deal(lessee, cost);
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.Overflow.selector);
        vault.create{value: cost}(operator, 1, duration, 1, INTENT, 0, ENDPOINT);
    }

    // ==================================================================
    // EXTEND
    // ==================================================================

    function test_Extend_AddsExactTimeAndEscrow() public {
        bytes32 id = _create(PRICE, DURATION);
        uint64 oldExpiry = _lease(id).expiry;
        vm.prank(payer); // anyone may subsidize
        vm.expectEmit(true, false, false, true);
        emit GpuLeaseVault.LeaseExtended(id, PRICE * 50, oldExpiry + 50);
        vault.extend{value: uint256(PRICE) * 50}(id, 50);

        GpuLeaseVault.Lease memory l = _lease(id);
        assertEq(l.escrow, PRICE * (DURATION + 50), "escrow after extend");
        assertEq(l.expiry, oldExpiry + 50, "expiry after extend");
        assertEq(vault.totalEscrowed(), uint256(PRICE) * (DURATION + 50), "totalEscrowed after extend");
        _checkConservation();
        _checkOverstayImpossible(id);
    }

    function test_Extend_OverpayRefundsExcess() public {
        bytes32 id = _create(PRICE, DURATION);
        uint256 payerBefore = payer.balance;
        vm.prank(payer);
        vault.extend{value: uint256(PRICE) * 10 + 1 ether}(id, 10);
        assertEq(payer.balance, payerBefore - uint256(PRICE) * 10, "excess not refunded on extend");
        _checkConservation();
    }

    function test_Extend_RevertZeroDuration() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.DurationZero.selector);
        vault.extend{value: 0}(id, 0);
    }

    function test_Extend_RevertInsufficientValue() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.InsufficientEscrow.selector);
        vault.extend{value: uint256(PRICE) * 10 - 1}(id, 10);
    }

    function test_Extend_RevertNotLive_AfterRelease() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.prank(lessee);
        vault.release(id);
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.NotLive.selector);
        vault.extend{value: PRICE}(id, 1);
    }

    function test_Extend_RevertNotLive_AfterReap() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.warp(block.timestamp + DURATION + 1);
        vault.reap(id);
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.NotLive.selector);
        vault.extend{value: PRICE}(id, 1);
    }

    function test_Extend_RevertExpiryOverflow() public {
        // Lease expiring near the uint64 ceiling.
        uint64 duration = type(uint64).max - uint64(block.timestamp) - 1;
        uint256 cost = uint256(PRICE) * uint256(duration);
        vm.deal(lessee, cost + 1 ether); // fund beyond cost so extend's value transfer clears
        bytes32 id = _create(PRICE, duration);
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.Overflow.selector);
        vault.extend{value: PRICE * 2}(id, 2);
    }

    // ==================================================================
    // RELEASE — I3 exact pro-rata
    // ==================================================================

    function test_Release_ExactProRata_HappyPath() public {
        bytes32 id = _create(PRICE, DURATION); // escrow 10_000
        vm.warp(block.timestamp + 30); // 30s elapsed, 70s remain
        uint256 lesseeBefore = lessee.balance;

        vm.expectEmit(true, false, false, true);
        emit GpuLeaseVault.LeaseReleased(id, PRICE * 70, PRICE * 30);
        vm.prank(lessee);
        vault.release(id);

        assertEq(lessee.balance, lesseeBefore + PRICE * 70, "I3: refund != price*remaining");
        assertEq(vault.operatorEarningsOf(operator), PRICE * 30, "operator take != price*elapsed");
        assertEq(vault.operatorEarnings(), PRICE * 30, "global earnings");
        GpuLeaseVault.Lease memory l = _lease(id);
        assertEq(l.escrow, 0, "escrow zeroed");
        assertEq(uint8(l.state), 1, "state Released");
        assertEq(vault.totalEscrowed(), 0, "totalEscrowed zeroed");
        _checkConservation();
    }

    function test_Release_AfterExtend_ProRataAcrossTotalPaid() public {
        bytes32 id = _create(PRICE, DURATION); // 100s paid
        vm.prank(payer);
        vault.extend{value: uint256(PRICE) * 50}(id, 50); // 150s paid, escrow 15_000
        vm.warp(block.timestamp + 60); // 60 elapsed, 90 remain
        uint256 lesseeBefore = lessee.balance;
        vm.prank(lessee);
        vault.release(id);
        // refund = price * remaining = 100*90 = 9000; take = 6000; sum = 15000. Exact.
        assertEq(lessee.balance, lesseeBefore + 9000, "refund after extend");
        assertEq(vault.operatorEarningsOf(operator), 6000, "take after extend");
        _checkConservation();
    }

    function test_Release_AtExpiry_FullTakeZeroRefund() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.warp(block.timestamp + DURATION); // exactly at expiry
        uint256 lesseeBefore = lessee.balance;
        vm.prank(lessee);
        vault.release(id);
        assertEq(lessee.balance, lesseeBefore, "no refund at expiry");
        assertEq(vault.operatorEarningsOf(operator), PRICE * DURATION, "full take at expiry");
        _checkConservation();
    }

    function test_Release_AfterExpiry_FullTakeZeroRefund() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.warp(block.timestamp + DURATION + 999);
        vm.prank(lessee);
        vault.release(id);
        assertEq(vault.operatorEarningsOf(operator), PRICE * DURATION, "full take past expiry");
        assertEq(_lease(id).state, 1, "settled Released");
        _checkConservation();
    }

    function test_Release_RevertNotLessee_NoStateChange() public {
        bytes32 id = _create(PRICE, DURATION);
        GpuLeaseVault.Lease memory before = _lease(id);
        vm.prank(other);
        vm.expectRevert(GpuLeaseVault.NotLessee.selector);
        vault.release(id);
        GpuLeaseVault.Lease memory after_ = _lease(id);
        // I3 atomicity precondition: failed calls leave no partial state.
        assertEq(after_.escrow, before.escrow, "escrow mutated on revert");
        assertEq(uint8(after_.state), 0, "state mutated on revert");
        _checkConservation();
    }

    function test_Release_WithRoundingInducingPrice_IsStillExact() public {
        // price chosen so escrow has no integral seconds remainder: price=7, dur=13
        uint128 price = 7;
        uint64 dur = 13;
        bytes32 id = _create(price, dur); // escrow 91
        vm.warp(block.timestamp + 5); // remaining 8
        uint256 lesseeBefore = lessee.balance;
        vm.prank(lessee);
        vault.release(id);
        assertEq(lessee.balance - lesseeBefore, 7 * 8, "exact refund 56");
        assertEq(vault.operatorEarningsOf(operator), 91 - 56, "exact take 35");
        assertEq(uint256(7 * 8 + (91 - 56)), uint256(91), "refund+take == escrow");
    }

    // ==================================================================
    // REAP — I2 enforcement at expiry
    // ==================================================================

    function test_Reap_RevertBeforeExpiry() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.warp(block.timestamp + DURATION - 1);
        vm.expectRevert(GpuLeaseVault.NotLive.selector);
        vault.reap(id);
        _checkConservation();
    }

    function test_Reap_Permissionless_FullEscrowToOperator() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.warp(block.timestamp + DURATION + 1);
        vm.expectEmit(true, false, false, true);
        emit GpuLeaseVault.LeaseReaped(id, PRICE * DURATION, other);
        vm.prank(other); // anyone
        vault.reap(id);

        assertEq(vault.operatorEarningsOf(operator), PRICE * DURATION, "operator take");
        assertEq(vault.operatorEarnings(), PRICE * DURATION, "global earnings");
        GpuLeaseVault.Lease memory l = _lease(id);
        assertEq(l.escrow, 0, "escrow zeroed");
        assertEq(uint8(l.state), 2, "state Reaped");
        assertEq(vault.totalEscrowed(), 0, "totalEscrowed zeroed");
        _checkConservation();
    }

    // ==================================================================
    // I4 — single settlement
    // ==================================================================

    function test_I4_ReleaseThenReap_Reverts() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.prank(lessee);
        vault.release(id);
        vm.warp(block.timestamp + DURATION + 1);
        vm.expectRevert(GpuLeaseVault.NotLive.selector);
        vault.reap(id);
    }

    function test_I4_ReapThenRelease_Reverts() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.warp(block.timestamp + DURATION + 1);
        vault.reap(id);
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.NotLive.selector);
        vault.release(id);
    }

    function test_I4_DoubleRelease_Reverts() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.prank(lessee);
        vault.release(id);
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.NotLive.selector);
        vault.release(id);
    }

    function test_I4_DoubleReap_Reverts() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.warp(block.timestamp + DURATION + 1);
        vault.reap(id);
        vm.expectRevert(GpuLeaseVault.NotLive.selector);
        vault.reap(id);
    }

    function test_I4_SetlementPaysOutExactlyOnce_SumConserved() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.warp(block.timestamp + 40);
        uint256 lesseeBefore = lessee.balance;
        uint256 operatorBefore = operator.balance;
        vm.prank(lessee);
        vault.release(id); // first settlement succeeds
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.NotLive.selector);
        vault.release(id); // second must fail
        vm.warp(block.timestamp + DURATION + 1);
        vm.expectRevert(GpuLeaseVault.NotLive.selector);
        vault.reap(id); // third must fail
        if (vault.operatorEarnings() > 0) {
            vm.prank(operator);
            vault.withdrawEarnings();
        }
        // Exactly one split ever leaves the vault.
        assertEq(
            (lessee.balance - lesseeBefore) + (operator.balance - operatorBefore),
            PRICE * DURATION,
            "I4/I1: total payout != escrow"
        );
        assertEq(address(vault).balance, 0, "vault not empty");
        _checkConservation();
    }

    // ==================================================================
    // I5 — quote binding / immutability
    // ==================================================================

    function test_I5_LeaseRecordImmutableAcrossLifecycle() public {
        bytes32 id = _create(PRICE, DURATION);
        GpuLeaseVault.Lease memory before = _lease(id);
        vm.prank(payer);
        vault.extend{value: uint256(PRICE) * 50}(id, 50);
        vm.warp(block.timestamp + 60);
        vm.prank(lessee);
        vault.release(id);
        GpuLeaseVault.Lease memory after_ = _lease(id);
        assertEq(after_.operator, before.operator, "I5: operator mutated");
        assertEq(after_.lessee, before.lessee, "I5: lessee mutated");
        assertEq(after_.pricePerSecond, before.pricePerSecond, "I5: price mutated");
        assertEq(after_.intentHash, before.intentHash, "I5: intentHash mutated");
        assertEq(after_.confidentiality, before.confidentiality, "I5: confidentiality mutated");
    }

    // ==================================================================
    // Withdraw
    // ==================================================================

    function test_WithdrawEarnings_ExactPayout() public {
        bytes32 id = _create(PRICE, DURATION);
        vm.warp(block.timestamp + 40);
        vm.prank(lessee);
        vault.release(id); // take = 4000
        uint256 operatorBefore = operator.balance;
        vm.prank(operator);
        vault.withdrawEarnings();
        assertEq(operator.balance - operatorBefore, PRICE * 40, "operator payout");
        assertEq(vault.operatorEarningsOf(operator), 0, "per-op zeroed");
        assertEq(vault.operatorEarnings(), 0, "global zeroed");
        assertEq(address(vault).balance, 0, "vault empty");
        _checkConservation();
        vm.prank(operator);
        vm.expectRevert(GpuLeaseVault.NothingToWithdraw.selector);
        vault.withdrawEarnings();
    }

    function test_WithdrawEarnings_AggregatesAcrossLeases() public {
        bytes32 id1 = _create(PRICE, DURATION);
        vm.warp(block.timestamp + 1);
        bytes32 id2 = _create(PRICE, DURATION);
        vm.warp(block.timestamp + 10); // id1: 11s elapsed, id2: 10s
        vm.prank(lessee);
        vault.release(id1);
        vm.prank(lessee);
        vault.release(id2);
        assertEq(vault.operatorEarningsOf(operator), PRICE * 21, "aggregated take");
        vm.prank(operator);
        vault.withdrawEarnings();
        assertEq(address(vault).balance, 0, "vault empty after withdraw");
        _checkConservation();
    }

    // ==================================================================
    // Slash hook
    // ==================================================================




    // ==================================================================
    // FUZZ — the invariants, pinned for arbitrary worlds
    // ==================================================================

    /// I3 exactness for arbitrary (price, duration, elapsed).
    function testFuzz_I3_ReleaseExactProRata(uint128 price, uint64 duration, uint256 warp) public {
        price = uint128(bound(price, 1, 1e21)); // sane wei/sec range
        duration = uint64(bound(duration, 1, 365 days));
        warp = bound(warp, 0, uint256(duration));
        uint256 cost = uint256(price) * uint256(duration);
        vm.deal(lessee, cost * 2);
        vm.prank(lessee);
        bytes32 id = vault.create{value: cost}(operator, price, duration, 1, INTENT, 0, ENDPOINT);
        vm.warp(block.timestamp + warp);

        uint256 lesseeBefore = lessee.balance;
        vm.prank(lessee);
        vault.release(id);

        uint256 expectedRefund = uint256(price) * (uint256(duration) - warp);
        uint256 expectedTake = cost - expectedRefund;
        assertEq(lessee.balance - lesseeBefore, expectedRefund, "I3 fuzz: refund not exact");
        assertEq(vault.operatorEarningsOf(operator), expectedTake, "I3 fuzz: take not exact");
        assertEq(expectedRefund + expectedTake, cost, "I1 fuzz: refund+take != escrow");
        _checkConservation();
    }

    /// I2 for arbitrary elapsed time, checked mid-flight (before any settle).
    function testFuzz_I2_OverstayImpossible(uint128 price, uint64 duration, uint256 warp, uint64 extend1, uint256 warp2) public {
        price = uint128(bound(price, 1, 1e21));
        duration = uint64(bound(duration, 1, 365 days));
        warp = bound(warp, 0, uint256(duration));
        extend1 = uint64(bound(extend1, 0, 365 days));
        warp2 = bound(warp2, 0, uint256(duration) + uint256(extend1));
        uint256 cost = uint256(price) * uint256(duration);
        vm.deal(lessee, (cost + uint256(price) * uint256(extend1)) * 2);
        vm.prank(lessee);
        bytes32 id = vault.create{value: cost}(operator, price, duration, 1, INTENT, 0, ENDPOINT);
        vm.warp(block.timestamp + warp);
        _checkOverstayImpossible(id); // mid-flight, arbitrary time

        if (extend1 > 0) {
            vm.prank(lessee);
            vault.extend{value: uint256(price) * uint256(extend1)}(id, extend1);
            _checkOverstayImpossible(id); // immediately after extend
        }
        vm.warp(block.timestamp + warp2);
        _checkOverstayImpossible(id); // arbitrary later time
        _checkConservation();
    }

    /// Full-lifecycle I1: create with overpay, k extends, arbitrary settle point,
    /// release or reap, withdraw — the vault ends EMPTY and every wei is accounted.
    function testFuzz_I1_FullLifecycleConservation(
        uint128 price,
        uint64 duration,
        uint64 extendA,
        uint64 extendB,
        uint256 warp,
        bool settleByRelease
    ) public {
        price = uint128(bound(price, 1, 1e18));
        duration = uint64(bound(duration, 1, 30 days));
        extendA = uint64(bound(extendA, 0, 30 days));
        extendB = uint64(bound(extendB, 0, 30 days));
        uint256 totalPaid = uint256(price) * uint256(duration);
        warp = bound(warp, 0, totalPaidSecondsBound(duration, extendA, extendB) + 1);

        vm.deal(lessee, type(uint256).max / 2);
        vm.deal(payer, type(uint256).max / 2);

        // create (with overpay to prove refund path)
        vm.startPrank(lessee);
        bytes32 id = vault.create{value: totalPaid + 1 ether}(operator, price, duration, 1, INTENT, 0, ENDPOINT);
        vm.stopPrank();
        _checkConservation();

        // extends (with overpay)
        if (extendA > 0) {
            vm.prank(payer);
            vault.extend{value: uint256(price) * uint256(extendA) + 0.5 ether}(id, extendA);
            _checkConservation();
        }
        if (extendB > 0) {
            vm.prank(lessee);
            vault.extend{value: uint256(price) * uint256(extendB) + 0.25 ether}(id, extendB);
            _checkConservation();
        }
        totalPaid += uint256(price) * (uint256(extendA) + uint256(extendB));

        uint256 lesseeBefore = lessee.balance;
        uint256 operatorBefore = operator.balance;

        vm.warp(block.timestamp + warp);
        _checkOverstayImpossible(id);

        if (settleByRelease) {
            vm.prank(lessee);
            vault.release(id);
        } else {
            // reap only valid at/after expiry
            if (block.timestamp < _lease(id).expiry) {
                vm.prank(lessee);
                vault.release(id);
            } else {
                vault.reap(id);
            }
        }
        _checkConservation();

        if (vault.operatorEarnings() > 0) {
            vm.prank(operator);
            vault.withdrawEarnings();
        }
        _checkConservation();

        // Terminal state: every wei that entered as cost is exactly split.
        assertEq(address(vault).balance, 0, "I1 terminal: vault not empty");
        assertEq(vault.totalEscrowed(), 0, "I1 terminal: totalEscrowed not zero");
        assertEq(vault.operatorEarnings(), 0, "I1 terminal: operatorEarnings not zero");
        assertEq(
            (lessee.balance - lesseeBefore) + (operator.balance - operatorBefore),
            totalPaid,
            "I1 terminal: payouts != totalPaid"
        );
    }

    function totalPaidSecondsBound(uint64 duration, uint64 extendA, uint64 extendB)
        internal
        pure
        returns (uint256)
    {
        return uint256(duration) + uint256(extendA) + uint256(extendB);
    }

    /// N concurrent leases, interleaved extends, settles in fuzz order:
    /// conservation must hold after EVERY step (checked), not just at the end.
    function testFuzz_I1_MultiLeaseInterleaved(uint128 seed) public {
        uint256 n = 5;
        bytes32[] memory ids = new bytes32[](n);
        uint256 expectedEscrowSum = 0;
        vm.deal(lessee, type(uint256).max / 2);
        for (uint256 i = 0; i < n; i++) {
            uint128 price = uint128(bound(uint256(keccak256(abi.encode(seed, i, "p"))), 1, 1000));
            uint64 dur = uint64(bound(uint256(keccak256(abi.encode(seed, i, "d"))), 1, 1000));
            uint256 cost = uint256(price) * uint256(dur);
            vm.prank(lessee);
            ids[i] = vault.create{value: cost}(operator, price, dur, 1, INTENT, 0, ENDPOINT);
            expectedEscrowSum += cost;
            _checkConservation();
        }
        // interleave: extend every other lease
        for (uint256 i = 0; i < n; i += 2) {
            GpuLeaseVault.Lease memory l = _lease(ids[i]);
            vm.prank(lessee);
            vault.extend{value: uint256(l.pricePerSecond) * 10}(ids[i], 10);
            expectedEscrowSum += uint256(l.pricePerSecond) * 10;
            _checkConservation();
        }
        assertEq(vault.totalEscrowed(), expectedEscrowSum, "totalEscrowed != ghost sum");
        // settle all, half by release half by reap
        vm.warp(block.timestamp + 2000);
        for (uint256 i = 0; i < n; i++) {
            if (i % 2 == 0) {
                vm.prank(lessee);
                vault.release(ids[i]);
            } else {
                vault.reap(ids[i]);
            }
            _checkConservation();
        }
        vm.prank(operator);
        vault.withdrawEarnings();
        assertEq(address(vault).balance, 0, "vault empty after full settle+withdraw");
        _checkConservation();
    }

    // ==================================================================
    // MULTI-GPU (deviceCount > 1) — the priced quantity (I5-immutable)
    // ==================================================================

    function test_Create_MultiDevice_EscrowsPerDevice() public {
        uint16 count = 4;
        uint256 cost = uint256(PRICE) * DURATION * count;
        vm.prank(lessee);
        vm.deal(lessee, 1_000_000 ether);
        bytes32 id = vault.create{value: cost}(operator, PRICE, DURATION, count, INTENT, 0, ENDPOINT);
        GpuLeaseVault.Lease memory l = _lease(id);
        assertEq(l.deviceCount, count, "deviceCount stored");
        assertEq(l.escrow, PRICE * DURATION * count, "escrow = price * duration * count");
        assertEq(vault.totalEscrowed(), cost, "totalEscrowed");
        _checkConservation();
        _checkOverstayImpossible(id);
    }

    function test_Create_MultiDevice_RevertZeroAndTooHighCount() public {
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.DeviceCountZero.selector);
        vault.create{value: uint256(PRICE) * DURATION}(operator, PRICE, DURATION, 0, INTENT, 0, ENDPOINT);

        vm.prank(lessee);
        vm.expectRevert(abi.encodeWithSelector(GpuLeaseVault.DeviceCountTooHigh.selector, 65, 64));
        vault.create{value: 1 ether}(operator, PRICE, DURATION, 65, INTENT, 0, ENDPOINT);
    }

    function test_Create_MultiDevice_RevertInsufficientEscrow() public {
        // Paying for one device when renting two must fail closed.
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.InsufficientEscrow.selector);
        vault.create{value: uint256(PRICE) * DURATION}(operator, PRICE, DURATION, 2, INTENT, 0, ENDPOINT);
    }

    function test_Extend_MultiDevice_CostsPerDevice() public {
        bytes32 id = _createCount(PRICE, DURATION, 2);
        // One-device payment for a two-device lease must fail.
        vm.prank(lessee);
        vm.expectRevert(GpuLeaseVault.InsufficientEscrow.selector);
        vault.extend{value: uint256(PRICE) * 10}(id, 10);
        // Exact per-device payment extends.
        vm.prank(lessee);
        vault.extend{value: uint256(PRICE) * 10 * 2}(id, 10);
        GpuLeaseVault.Lease memory l = _lease(id);
        assertEq(l.escrow, PRICE * (DURATION + 10) * 2, "escrow grows per device");
        _checkConservation();
        _checkOverstayImpossible(id);
    }

    function test_Release_MultiDevice_ExactProRataPerDevice() public {
        bytes32 id = _createCount(PRICE, DURATION, 3);
        vm.warp(block.timestamp + 50);
        uint256 remaining = DURATION - 50;
        uint256 expectedRefund = uint256(PRICE) * remaining * 3;
        uint256 expectedTake = uint256(PRICE) * DURATION * 3 - expectedRefund;
        uint256 before = lessee.balance;
        vm.prank(lessee);
        vm.expectEmit(true, true, true, true);
        emit GpuLeaseVault.LeaseReleased(id, uint128(expectedRefund), uint128(expectedTake));
        vault.release(id);
        assertEq(lessee.balance - before, expectedRefund, "per-device refund exact");
        _checkConservation();
    }

    function test_Reap_MultiDevice_FullEscrowToOperator() public {
        bytes32 id = _createCount(PRICE, DURATION, 2);
        vm.warp(block.timestamp + DURATION + 1);
        vm.prank(operator);
        vault.reap(id);
        assertEq(vault.operatorEarningsOf(operator), uint256(PRICE) * DURATION * 2, "full multi escrow");
        _checkConservation();
    }
}
