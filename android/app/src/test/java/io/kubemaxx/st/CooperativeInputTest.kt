package io.kubemaxx.st

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class CooperativeInputTest {
    @Test
    fun onlyUnavailableOwnershipBlocksKeyboardTextAndMouse() {
        assertFalse(controllerOwnershipAllowsInput(ControllerOwnership.UNAVAILABLE))
        assertTrue(controllerOwnershipAllowsInput(ControllerOwnership.AVAILABLE))
        assertTrue(controllerOwnershipAllowsInput(ControllerOwnership.OWNED_BY_YOU))
        assertTrue(controllerOwnershipAllowsInput(ControllerOwnership.OWNED_BY_OTHER))
    }

    @Test
    fun reliablePredictionReseedsWhenOwnershipMovesToAnotherClient() {
        assertTrue(
            shouldReseedPredictionForOwnership(
                ControllerOwnership.OWNED_BY_YOU,
                ControllerOwnership.OWNED_BY_OTHER,
                cursorPositionReliable = true,
            ),
        )
        assertFalse(
            shouldReseedPredictionForOwnership(
                ControllerOwnership.OWNED_BY_YOU,
                ControllerOwnership.OWNED_BY_OTHER,
                cursorPositionReliable = false,
            ),
        )
        assertFalse(
            shouldReseedPredictionForOwnership(
                ControllerOwnership.OWNED_BY_OTHER,
                ControllerOwnership.OWNED_BY_OTHER,
                cursorPositionReliable = true,
            ),
        )
    }
}
