package com.plugin.vpnservice

import org.junit.Test

import org.junit.Assert.*

class ExampleUnitTest {
    private data class FakeInterface(var closed: Boolean = false)

    @Test
    fun oldInterfaceClosesOnlyAfterLatestGenerationIsInstalled() {
        val lifecycle = VpnInterfaceLifecycle<FakeInterface>(closeInterface = { it.closed = true })
        val oldInterface = FakeInterface()
        val candidate = FakeInterface()

        assertTrue(lifecycle.reserveGeneration(1))
        assertTrue(lifecycle.addCandidate(1, oldInterface))
        assertTrue(lifecycle.complete(1))
        assertTrue(lifecycle.reserveGeneration(2))
        assertTrue(lifecycle.addCandidate(2, candidate))
        assertFalse(oldInterface.closed)

        assertTrue(lifecycle.complete(2, installed = false))
        assertFalse(oldInterface.closed)
        assertFalse(candidate.closed)
        assertFalse(lifecycle.complete(1))

        assertTrue(lifecycle.complete(2))
        assertTrue(oldInterface.closed)
        assertFalse(candidate.closed)
        assertFalse(lifecycle.complete(1))

        lifecycle.closeAll()
        assertTrue(candidate.closed)
    }

    @Test
    fun duplicateGenerationIsRejectedWithoutReplacingCandidate() {
        val lifecycle = VpnInterfaceLifecycle<FakeInterface>(closeInterface = { it.closed = true })
        val candidate = FakeInterface()

        assertTrue(lifecycle.reserveGeneration(1))
        assertTrue(lifecycle.addCandidate(1, candidate))
        assertFalse(lifecycle.reserveGeneration(1))
        assertFalse(lifecycle.addCandidate(1, FakeInterface()))
        assertTrue(lifecycle.complete(1))
        assertFalse(candidate.closed)
    }

    @Test
    fun staleRequestIsRejectedBeforeCandidateCreation() {
        val lifecycle = VpnInterfaceLifecycle<FakeInterface>(closeInterface = { it.closed = true })
        assertTrue(lifecycle.reserveGeneration(12))
        assertFalse(lifecycle.reserveGeneration(11))
        assertEquals(12L, lifecycle.highWaterGeneration())
    }

    @Test
    fun delayedOlderCandidateCannotReplaceNewerCompletion() {
        val lifecycle = VpnInterfaceLifecycle<FakeInterface>(closeInterface = { it.closed = true })
        val older = FakeInterface()
        val newer = FakeInterface()

        assertTrue(lifecycle.reserveGeneration(19))
        assertTrue(lifecycle.reserveGeneration(20))
        assertTrue(lifecycle.addCandidate(20, newer))
        assertTrue(lifecycle.complete(20))
        assertFalse(lifecycle.addCandidate(19, older))
        older.closeInterface()
        assertFalse(lifecycle.complete(19))
        assertFalse(newer.closed)
        assertTrue(older.closed)
    }

    @Test
    fun closingInterfacesRetainsGenerationHighWater() {
        val lifecycle = VpnInterfaceLifecycle<FakeInterface>(closeInterface = { it.closed = true })
        assertTrue(lifecycle.reserveGeneration(5))
        lifecycle.closeAll()
        assertEquals(5L, lifecycle.highWaterGeneration())
        assertFalse(lifecycle.reserveGeneration(4))
        assertTrue(lifecycle.reserveGeneration(6))
    }

    private fun FakeInterface.closeInterface() {
        this.closed = true
    }
}
