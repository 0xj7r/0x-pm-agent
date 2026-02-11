"""Claude API client for fair value estimation.

Uses Opus to analyze markets and estimate probabilities.
Tracks API costs so the agent can pay for its own inference.
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass

import anthropic

from config import Config

logger = logging.getLogger(__name__)

# Opus pricing (per million tokens)
INPUT_COST_PER_M = 15.0
OUTPUT_COST_PER_M = 75.0


@dataclass
class FairValueEstimate:
    probability: float  # 0-1, estimated true probability of YES outcome
    confidence: float  # 0-1, how confident the model is
    reasoning: str
    api_cost_usd: float


class ClaudeClient:
    def __init__(self, config: Config):
        self.client = anthropic.Anthropic(api_key=config.ANTHROPIC_API_KEY)
        self.model = config.CLAUDE_MODEL
        self.total_cost_usd = 0.0

    def estimate_fair_value(
        self,
        question: str,
        description: str,
        current_yes_price: float,
        additional_context: str = "",
    ) -> FairValueEstimate:
        """Ask Claude to estimate the fair probability of a market outcome.

        Returns a FairValueEstimate with probability, confidence, and reasoning.
        """
        prompt = f"""You are a prediction market analyst. Estimate the TRUE probability of this outcome.

MARKET QUESTION: {question}

MARKET DESCRIPTION: {description}

CURRENT MARKET PRICE: YES = ${current_yes_price:.2f} (implying {current_yes_price * 100:.1f}% probability)

{f"ADDITIONAL CONTEXT:{chr(10)}{additional_context}" if additional_context else ""}

Analyze this carefully:
1. What is the base rate for this type of event?
2. What specific factors increase or decrease the probability?
3. Is the current market price reasonable, too high, or too low?
4. What is your estimated TRUE probability?

Respond in JSON format:
{{
    "probability": <float 0-1>,
    "confidence": <float 0-1>,
    "reasoning": "<brief explanation>"
}}

Be calibrated. If you're uncertain, reflect that in a lower confidence score.
Do NOT anchor to the current market price - estimate independently."""

        try:
            response = self.client.messages.create(
                model=self.model,
                max_tokens=512,
                messages=[{"role": "user", "content": prompt}],
            )

            # Calculate cost
            input_tokens = response.usage.input_tokens
            output_tokens = response.usage.output_tokens
            cost = (
                input_tokens * INPUT_COST_PER_M / 1_000_000
                + output_tokens * OUTPUT_COST_PER_M / 1_000_000
            )
            self.total_cost_usd += cost

            # Parse response
            text = response.content[0].text.strip()
            # Handle markdown code blocks
            if text.startswith("```"):
                text = text.split("\n", 1)[1].rsplit("```", 1)[0].strip()

            data = json.loads(text)

            estimate = FairValueEstimate(
                probability=float(data["probability"]),
                confidence=float(data["confidence"]),
                reasoning=data.get("reasoning", ""),
                api_cost_usd=cost,
            )

            logger.info(
                f"Fair value estimate: {estimate.probability:.2f} "
                f"(confidence: {estimate.confidence:.2f}, cost: ${cost:.4f})"
            )

            return estimate

        except json.JSONDecodeError as e:
            logger.error(f"Failed to parse Claude response: {e}")
            return FairValueEstimate(
                probability=current_yes_price,
                confidence=0.0,
                reasoning=f"Parse error: {e}",
                api_cost_usd=0.0,
            )
        except Exception as e:
            logger.error(f"Claude API error: {e}")
            return FairValueEstimate(
                probability=current_yes_price,
                confidence=0.0,
                reasoning=f"API error: {e}",
                api_cost_usd=0.0,
            )

    def estimate_weather_market(
        self,
        question: str,
        description: str,
        current_yes_price: float,
        ensemble_summary: str,
    ) -> FairValueEstimate:
        """Specialized weather market estimation with ensemble data context."""
        return self.estimate_fair_value(
            question=question,
            description=description,
            current_yes_price=current_yes_price,
            additional_context=(
                f"NOAA GEFS ENSEMBLE FORECAST DATA:\n{ensemble_summary}\n\n"
                "This is data from 21 independent weather model runs. "
                "The ensemble probability is mathematically derived and should be "
                "weighted heavily in your estimate."
            ),
        )
