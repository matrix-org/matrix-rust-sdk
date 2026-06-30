// Copyright 2026 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Password strength estimation is powered by zxcvbn, which evaluates passwords via pattern
// matching: dictionary words, keyboard walks, repeats, dates, l33t speak, etc.
//
// zxcvbn produces a numeric score (log₁₀ of estimated guesses needed to crack the password),
// accounting for both brute force and pattern-based attacks — whichever requires fewer guesses.
// We do not use zxcvbn's own ranking (Score::Zero–Four), because the library was written over
// a decade ago and its thresholds have not been updated to reflect modern hardware attack rates.
// Instead, the caller supplies PasswordStrengthThresholds, which define the minimum score
// required to achieve each ranking level. The final ranking is derived solely from that score
// against the thresholds, giving callers full control over what constitutes an acceptable password.

use zxcvbn::feedback::{Suggestion as ZxcvbnSuggestion, Warning as ZxcvbnWarning};

/// A ranking representing the estimated strength of a password, ranging from
/// `VeryWeak` (easily guessable) to `VeryStrong` (highly resistant to attack).
#[derive(uniffi::Enum)]
pub enum PasswordStrengthRanking {
    VeryWeak,
    Weak,
    Fair,
    Strong,
    VeryStrong,
}

/// A warning explaining what is wrong with the password.
#[derive(uniffi::Enum)]
pub enum PasswordStrengthWarning {
    StraightRowsOfKeysAreEasyToGuess,
    ShortKeyboardPatternsAreEasyToGuess,
    RepeatsLikeAaaAreEasyToGuess,
    RepeatsLikeAbcAbcAreOnlySlightlyHarderToGuess,
    ThisIsATop10Password,
    ThisIsATop100Password,
    ThisIsACommonPassword,
    ThisIsSimilarToACommonlyUsedPassword,
    SequencesLikeAbcAreEasyToGuess,
    RecentYearsAreEasyToGuess,
    AWordByItselfIsEasyToGuess,
    DatesAreOftenEasyToGuess,
    NamesAndSurnamesByThemselvesAreEasyToGuess,
    CommonNamesAndSurnamesAreEasyToGuess,
}

impl From<ZxcvbnWarning> for PasswordStrengthWarning {
    fn from(warning: ZxcvbnWarning) -> Self {
        match warning {
            ZxcvbnWarning::StraightRowsOfKeysAreEasyToGuess => {
                Self::StraightRowsOfKeysAreEasyToGuess
            }
            ZxcvbnWarning::ShortKeyboardPatternsAreEasyToGuess => {
                Self::ShortKeyboardPatternsAreEasyToGuess
            }
            ZxcvbnWarning::RepeatsLikeAaaAreEasyToGuess => Self::RepeatsLikeAaaAreEasyToGuess,
            ZxcvbnWarning::RepeatsLikeAbcAbcAreOnlySlightlyHarderToGuess => {
                Self::RepeatsLikeAbcAbcAreOnlySlightlyHarderToGuess
            }
            ZxcvbnWarning::ThisIsATop10Password => Self::ThisIsATop10Password,
            ZxcvbnWarning::ThisIsATop100Password => Self::ThisIsATop100Password,
            ZxcvbnWarning::ThisIsACommonPassword => Self::ThisIsACommonPassword,
            ZxcvbnWarning::ThisIsSimilarToACommonlyUsedPassword => {
                Self::ThisIsSimilarToACommonlyUsedPassword
            }
            ZxcvbnWarning::SequencesLikeAbcAreEasyToGuess => Self::SequencesLikeAbcAreEasyToGuess,
            ZxcvbnWarning::RecentYearsAreEasyToGuess => Self::RecentYearsAreEasyToGuess,
            ZxcvbnWarning::AWordByItselfIsEasyToGuess => Self::AWordByItselfIsEasyToGuess,
            ZxcvbnWarning::DatesAreOftenEasyToGuess => Self::DatesAreOftenEasyToGuess,
            ZxcvbnWarning::NamesAndSurnamesByThemselvesAreEasyToGuess => {
                Self::NamesAndSurnamesByThemselvesAreEasyToGuess
            }
            ZxcvbnWarning::CommonNamesAndSurnamesAreEasyToGuess => {
                Self::CommonNamesAndSurnamesAreEasyToGuess
            }
        }
    }
}

/// A suggestion to help the user choose a stronger password.
#[derive(uniffi::Enum)]
pub enum PasswordStrengthSuggestion {
    UseAFewWordsAvoidCommonPhrases,
    NoNeedForSymbolsDigitsOrUppercaseLetters,
    AddAnotherWordOrTwo,
    CapitalizationDoesntHelpVeryMuch,
    AllUppercaseIsAlmostAsEasyToGuessAsAllLowercase,
    ReversedWordsArentMuchHarderToGuess,
    PredictableSubstitutionsDontHelpVeryMuch,
    UseALongerKeyboardPatternWithMoreTurns,
    AvoidRepeatedWordsAndCharacters,
    AvoidSequences,
    AvoidRecentYears,
    AvoidYearsThatAreAssociatedWithYou,
    AvoidDatesAndYearsThatAreAssociatedWithYou,
}

impl From<ZxcvbnSuggestion> for PasswordStrengthSuggestion {
    fn from(suggestion: ZxcvbnSuggestion) -> Self {
        match suggestion {
            ZxcvbnSuggestion::UseAFewWordsAvoidCommonPhrases => {
                Self::UseAFewWordsAvoidCommonPhrases
            }
            ZxcvbnSuggestion::NoNeedForSymbolsDigitsOrUppercaseLetters => {
                Self::NoNeedForSymbolsDigitsOrUppercaseLetters
            }
            ZxcvbnSuggestion::AddAnotherWordOrTwo => Self::AddAnotherWordOrTwo,
            ZxcvbnSuggestion::CapitalizationDoesntHelpVeryMuch => {
                Self::CapitalizationDoesntHelpVeryMuch
            }
            ZxcvbnSuggestion::AllUppercaseIsAlmostAsEasyToGuessAsAllLowercase => {
                Self::AllUppercaseIsAlmostAsEasyToGuessAsAllLowercase
            }
            ZxcvbnSuggestion::ReversedWordsArentMuchHarderToGuess => {
                Self::ReversedWordsArentMuchHarderToGuess
            }
            ZxcvbnSuggestion::PredictableSubstitutionsDontHelpVeryMuch => {
                Self::PredictableSubstitutionsDontHelpVeryMuch
            }
            ZxcvbnSuggestion::UseALongerKeyboardPatternWithMoreTurns => {
                Self::UseALongerKeyboardPatternWithMoreTurns
            }
            ZxcvbnSuggestion::AvoidRepeatedWordsAndCharacters => {
                Self::AvoidRepeatedWordsAndCharacters
            }
            ZxcvbnSuggestion::AvoidSequences => Self::AvoidSequences,
            ZxcvbnSuggestion::AvoidRecentYears => Self::AvoidRecentYears,
            ZxcvbnSuggestion::AvoidYearsThatAreAssociatedWithYou => {
                Self::AvoidYearsThatAreAssociatedWithYou
            }
            ZxcvbnSuggestion::AvoidDatesAndYearsThatAreAssociatedWithYou => {
                Self::AvoidDatesAndYearsThatAreAssociatedWithYou
            }
        }
    }
}

/// Verbal feedback to help the user choose a stronger password.
#[derive(uniffi::Record)]
pub struct PasswordStrengthFeedback {
    /// An optional warning explaining what is wrong with the password.
    pub warning: Option<PasswordStrengthWarning>,
    /// A possibly-empty list of actionable suggestions.
    pub suggestions: Vec<PasswordStrengthSuggestion>,
}

/// The full result of a password strength estimation.
#[derive(uniffi::Record)]
pub struct PasswordStrengthEstimate {
    /// Overall strength ranking from VeryWeak to VeryStrong.
    pub ranking: PasswordStrengthRanking,
    /// Estimated number of guesses needed to crack the password.
    pub guesses: u64,
    /// A numeric score derived from the order of magnitude of `guesses`
    /// (i.e. log base 10).
    pub score: f64,
    /// Verbal feedback to help choose a better password. Only set when the
    /// ranking is Fair or below.
    pub feedback: Option<PasswordStrengthFeedback>,
}

/// Minimum `score` (log₁₀ of estimated guesses) required to achieve each
/// ranking level. Any score below `weak` is ranked `VeryWeak`.
#[derive(uniffi::Record)]
pub struct PasswordStrengthThresholds {
    /// Minimum score to achieve `Weak`.
    pub weak: f64,
    /// Minimum score to achieve `Fair`.
    pub fair: f64,
    /// Minimum score to achieve `Strong`.
    pub strong: f64,
    /// Minimum score to achieve `VeryStrong`.
    pub very_strong: f64,
}

impl PasswordStrengthThresholds {
    fn ranking_for_score(&self, score: f64) -> PasswordStrengthRanking {
        if score >= self.very_strong {
            PasswordStrengthRanking::VeryStrong
        } else if score >= self.strong {
            PasswordStrengthRanking::Strong
        } else if score >= self.fair {
            PasswordStrengthRanking::Fair
        } else if score >= self.weak {
            PasswordStrengthRanking::Weak
        } else {
            PasswordStrengthRanking::VeryWeak
        }
    }
}

/// Estimates password strength using caller-supplied thresholds.
///
/// Construct once with your desired thresholds, then call `estimate` for each
/// password without having to re-supply the thresholds every time.
#[derive(uniffi::Object)]
pub struct PasswordStrengthEstimator {
    thresholds: PasswordStrengthThresholds,
}

#[matrix_sdk_ffi_macros::export]
impl PasswordStrengthEstimator {
    #[uniffi::constructor]
    pub fn new(thresholds: PasswordStrengthThresholds) -> Self {
        Self { thresholds }
    }

    /// Creates an estimator using zxcvbn's original thresholds.
    #[uniffi::constructor]
    pub fn with_zxcvbn_defaults() -> Self {
        Self {
            thresholds: PasswordStrengthThresholds {
                weak: 3.0,      // 10^3
                fair: 6.0,      // 10^6
                strong: 8.0,    // 10^8
                very_strong: 10.0, // 10^10
            },
        }
    }

    /// Creates an estimator using thresholds tuned for modern hardware (2025).
    /// Values derived from determining entropy from the chart at https://www.hivesystems.com/blog/are-your-passwords-in-the-green
    #[uniffi::constructor]
    pub fn with_modern_defaults2025() -> Self {
        Self {
            thresholds: PasswordStrengthThresholds {
                weak: 11.0,
                fair: 16.5,
                strong: 22.0,
                very_strong: 25.5,
            },
        }
    }

    /// Estimates the strength of `password`.
    ///
    /// Optionally, pass a list of `user_inputs` (e.g. username, email address)
    /// so that the estimator can penalize passwords that contain personal
    /// information.
    ///
    /// The returned ranking is derived from the configured thresholds applied
    /// to the estimated guess count, which already accounts for pattern-based
    /// attacks.
    pub fn estimate(&self, password: String, user_inputs: Vec<String>) -> PasswordStrengthEstimate {
        let inputs: Vec<&str> = user_inputs.iter().map(String::as_str).collect();
        let entropy = zxcvbn::zxcvbn(&password, &inputs);

        let feedback = entropy.feedback().map(|f| PasswordStrengthFeedback {
            warning: f.warning().map(PasswordStrengthWarning::from),
            suggestions: f
                .suggestions()
                .iter()
                .copied()
                .map(PasswordStrengthSuggestion::from)
                .collect(),
        });

        let ranking = self.thresholds.ranking_for_score(entropy.guesses_log10());

        PasswordStrengthEstimate {
            ranking,
            guesses: entropy.guesses(),
            score: entropy.guesses_log10(),
            feedback,
        }
    }
}
