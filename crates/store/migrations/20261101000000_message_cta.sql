-- A release note's button in the Signal chat (Desktop's callToActionId and
-- callToActionText): JSON CallToAction.
ALTER TABLE messages ADD COLUMN cta TEXT;
