`SessionVerificationController::start_sas_verification` is now a no-op if the other
side already started the SAS verification, instead of replacing the already accepted
SAS flow with a new one.
