import React, { useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  ArrowLeft,
  ArrowRight,
  Bookmark,
  BriefcaseBusiness,
  Check,
  ClipboardList,
  ExternalLink,
  FileText,
  LayoutDashboard,
  Loader2,
  Plus,
  Search,
  Sparkles,
  Trash2,
  Upload,
  User,
  X,
} from 'lucide-react';
import './styles.css';

const STORAGE_KEY = 'opportunity-intake-v1';
const SUBMITTED_PROFILE_KEY = 'opportunity-search-profile-v1';
const RESUME_DB = 'opportunity-intake-files';
const RESUME_STORE = 'files';
const RESUME_KEY = 'resume';

const defaultSources = [
  { id: 'indeed', label: 'Indeed', url: 'https://www.indeed.com' },
  { id: 'google-jobs', label: 'Google Jobs', url: 'https://www.google.com/search?q=jobs' },
  { id: 'linkedin', label: 'LinkedIn', url: 'https://www.linkedin.com/jobs' },
];

const sourceSuggestions = [
  ...defaultSources,
  { id: 'handshake', label: 'Handshake', url: 'https://joinhandshake.com' },
  { id: 'wellfound', label: 'Wellfound', url: 'https://wellfound.com/jobs' },
  { id: 'greenhouse', label: 'Greenhouse', url: 'https://boards.greenhouse.io' },
];

const defaultProfile = {
  opportunityType: 'Full-time',
  workStyle: 'Flexible / open to any',
  location: '',
  industries: '',
  roles: '',
  experienceLevel: 'Entry-level',
  educationStatus: '',
  personalDescription: '',
};

function App() {
  const [profile, setProfile] = usePersistentState('profile', defaultProfile);
  const [sources, setSources] = usePersistentState('sources', defaultSources);
  const [savedOpportunities, setSavedOpportunities] = usePersistentState('savedOpportunities', []);
  const [applications, setApplications] = usePersistentState('applications', []);
  const [activeTab, setActiveTab] = useState('sources');
  const [sourceInput, setSourceInput] = useState('');
  const [resume, setResume] = useState(null);
  const [savedAt, setSavedAt] = useState('');
  const [searchSavedAt, setSearchSavedAt] = useState('');
  const [searchState, setSearchState] = useState('idle');
  const [jobMatches, setJobMatches] = useState([]);
  const [sourceNotes, setSourceNotes] = useState([]);
  const [searchError, setSearchError] = useState('');
  const fileInputRef = useRef(null);

  useEffect(() => {
    let ignore = false;
    loadResume().then((storedResume) => {
      if (!ignore && storedResume) setResume(storedResume);
    });
    return () => {
      ignore = true;
    };
  }, []);

  useEffect(() => {
    setSavedAt(formatSavedAt(new Date()));
  }, [profile, sources, resume]);

  const completion = useMemo(() => {
    const requiredValues = [
      sources.length,
      profile.opportunityType,
      profile.workStyle,
      profile.location,
      profile.industries,
      profile.roles,
      profile.experienceLevel,
      profile.educationStatus,
      profile.personalDescription,
      resume?.name,
    ];
    return Math.round((requiredValues.filter(Boolean).length / requiredValues.length) * 100);
  }, [profile, resume, sources.length]);

  function updateProfile(field, value) {
    setProfile((current) => ({ ...current, [field]: value }));
  }

  function addSource(event) {
    event.preventDefault();
    const source = normalizeSource(sourceInput);
    if (!source) return;
    setSources((current) => {
      const exists = current.some(
        (item) =>
          item.label.toLowerCase() === source.label.toLowerCase() ||
          item.url.toLowerCase() === source.url.toLowerCase(),
      );
      return exists ? current : [...current, source];
    });
    setSourceInput('');
  }

  function addSuggestedSource(source) {
    setSources((current) =>
      current.some((item) => item.id === source.id || item.url === source.url)
        ? current
        : [...current, source],
    );
  }

  function removeSource(sourceId) {
    setSources((current) => current.filter((source) => source.id !== sourceId));
  }

  async function handleResumeUpload(event) {
    const [file] = event.target.files ?? [];
    if (!file) return;
    const nextResume = {
      file,
      name: file.name,
      size: file.size,
      type: file.type || 'Resume',
      updatedAt: new Date().toISOString(),
    };
    await saveResume(nextResume);
    setResume(nextResume);
    event.target.value = '';
  }

  async function removeResume() {
    await deleteResume();
    setResume(null);
  }

  function saveOpportunity(match) {
    const savedAt = new Date().toISOString();
    setSavedOpportunities((current) => {
      if (current.some((item) => item.id === match.id || item.url === match.url)) return current;
      return [{ ...match, savedAt }, ...current];
    });
  }

  function removeSavedOpportunity(opportunityId) {
    setSavedOpportunities((current) => current.filter((item) => item.id !== opportunityId));
  }

  function addApplication(opportunity) {
    const now = new Date().toISOString();
    setApplications((current) => {
      if (current.some((item) => item.id === opportunity.id || item.url === opportunity.url)) {
        return current;
      }
      return [
        {
          ...opportunity,
          status: 'Saved',
          appliedDate: '',
          followUpDate: '',
          contact: '',
          notes: '',
          createdAt: now,
        },
        ...current,
      ];
    });
  }

  function updateApplication(applicationId, field, value) {
    setApplications((current) =>
      current.map((item) => (item.id === applicationId ? { ...item, [field]: value } : item)),
    );
  }

  function removeApplication(applicationId) {
    setApplications((current) => current.filter((item) => item.id !== applicationId));
  }

  async function saveSearchProfile() {
    if (!readyToSearch) return;
    const timestamp = new Date();
    const searchProfile = {
      sources,
      profile,
      resume: resume
        ? {
            name: resume.name,
            size: resume.size,
            type: resume.type,
            updatedAt: resume.updatedAt,
          }
        : null,
      savedAt: timestamp.toISOString(),
    };
    window.localStorage.setItem(SUBMITTED_PROFILE_KEY, JSON.stringify(searchProfile));
    setSearchSavedAt(formatSavedAt(timestamp));
    setSearchState('searching');
    setSearchError('');
    setJobMatches([]);
    setSourceNotes([]);
    setActiveTab('listings');

    try {
      const response = await fetch('/api/opportunity-search', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(searchProfile),
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) {
        throw new Error(payload.error || 'Search failed before results were returned.');
      }
      setJobMatches(payload.matches ?? []);
      setSourceNotes(payload.sourceNotes ?? []);
      setSearchState('done');
    } catch (error) {
      setSearchError(error.message || 'Search failed before results were returned.');
      setSearchState('error');
    }
  }

  const readyToSearch =
    sources.length > 0 &&
    profile.location.trim() &&
    profile.industries.trim() &&
    profile.roles.trim() &&
    profile.educationStatus.trim();

  return (
    <main className="appShell">
      <section className="heroBand">
        <div className="brandMark" aria-hidden="true">
          <BriefcaseBusiness size={26} />
        </div>
        <div className="heroCopy">
          <p>Opportunity search builder</p>
          <h1>Tell the search what to watch for.</h1>
          <span>
            Choose job sites, answer the core questions, then add the personal context that
            makes a match useful.
          </span>
        </div>
        <div className="saveStatus" aria-live="polite">
          <Check size={16} />
          <span>{savedAt ? `Auto-saved ${savedAt}` : 'Auto-save is on'}</span>
        </div>
      </section>

      <nav className="topTabs" aria-label="Opportunity workspace">
        <button
          className={activeTab === 'sources' ? 'active' : ''}
          type="button"
          onClick={() => setActiveTab('sources')}
        >
          <Search size={18} />
          Sources
        </button>
        <button
          className={activeTab === 'questionnaire' ? 'active' : ''}
          type="button"
          onClick={() => setActiveTab('questionnaire')}
        >
          <ClipboardList size={18} />
          Questionnaire
        </button>
        <button
          className={activeTab === 'details' ? 'active' : ''}
          type="button"
          onClick={() => setActiveTab('details')}
        >
          <Sparkles size={18} />
          Details
        </button>
        <button
          className={activeTab === 'listings' ? 'active' : ''}
          type="button"
          onClick={() => setActiveTab('listings')}
        >
          <LayoutDashboard size={18} />
          Listings
        </button>
        <button
          className={activeTab === 'profile' ? 'active' : ''}
          type="button"
          onClick={() => setActiveTab('profile')}
        >
          <User size={18} />
          Profile
        </button>
      </nav>

      {['sources', 'questionnaire', 'details'].includes(activeTab) && (
      <section className="workspace setupWorkspace">
        <form className="intakePanel" onSubmit={(event) => event.preventDefault()}>
          {activeTab === 'sources' && (
          <section className="formSection" aria-labelledby="sources-heading">
            <div className="sectionHeader">
              <div>
                <span>Step 1</span>
                <h2 id="sources-heading">Sites to track</h2>
              </div>
              <strong>{sources.length} active</strong>
            </div>

            <div className="sourceSuggestions" aria-label="Suggested job sites">
              {sourceSuggestions.map((source) => {
                const active = sources.some((item) => item.id === source.id || item.url === source.url);
                return (
                  <button
                    className={active ? 'suggestion active' : 'suggestion'}
                    key={source.id}
                    type="button"
                    onClick={() => addSuggestedSource(source)}
                  >
                    {active && <Check size={15} />}
                    {source.label}
                  </button>
                );
              })}
            </div>

            <div className="sourceList">
              {sources.map((source) => (
                <div className="sourceItem" key={source.id}>
                  <Search size={17} />
                  <div>
                    <strong>{source.label}</strong>
                    <span>{source.url}</span>
                  </div>
                  <button
                    aria-label={`Remove ${source.label}`}
                    type="button"
                    onClick={() => removeSource(source.id)}
                  >
                    <X size={17} />
                  </button>
                </div>
              ))}
            </div>

            <div className="addSourceRow">
              <input
                value={sourceInput}
                onChange={(event) => setSourceInput(event.target.value)}
                placeholder="Add another job board or company career site"
                aria-label="Add a job source"
              />
              <button type="button" onClick={addSource} disabled={!sourceInput.trim()}>
                <Plus size={18} />
                Add
              </button>
            </div>
          </section>
          )}

          {activeTab === 'questionnaire' && (
          <section className="formSection" aria-labelledby="questionnaire-heading">
            <div className="sectionHeader">
              <div>
                <span>Step 2</span>
                <h2 id="questionnaire-heading">Generic questionnaire</h2>
              </div>
              <strong>{completion}% complete</strong>
            </div>

            <div className="questionGrid">
              <label>
                What type of opportunity are you looking for?
                <select
                  value={profile.opportunityType}
                  onChange={(event) => updateProfile('opportunityType', event.target.value)}
                >
                  <option>Full-time</option>
                  <option>Part-time</option>
                  <option>Internship</option>
                  <option>Contract</option>
                  <option>Fellowship</option>
                  <option>Not sure</option>
                </select>
              </label>

              <label>
                Style of work
                <select
                  value={profile.workStyle}
                  onChange={(event) => updateProfile('workStyle', event.target.value)}
                >
                  <option>Remote</option>
                  <option>Hybrid</option>
                  <option>In-person</option>
                  <option>Flexible / open to any</option>
                </select>
              </label>

              <label>
                Location
                <input
                  value={profile.location}
                  onChange={(event) => updateProfile('location', event.target.value)}
                  placeholder="City, state, country, or remote region"
                />
              </label>

              <label>
                What industries or fields are you interested in?
                <input
                  value={profile.industries}
                  onChange={(event) => updateProfile('industries', event.target.value)}
                  placeholder="Education, climate, health care, finance..."
                />
              </label>

              <label>
                What job titles or roles are you interested in?
                <input
                  value={profile.roles}
                  onChange={(event) => updateProfile('roles', event.target.value)}
                  placeholder="Analyst, coordinator, designer, software intern..."
                />
              </label>

              <label>
                What experience level are you looking for?
                <select
                  value={profile.experienceLevel}
                  onChange={(event) => updateProfile('experienceLevel', event.target.value)}
                >
                  <option>Entry-level</option>
                  <option>Internship / student</option>
                  <option>Associate</option>
                  <option>Mid-level</option>
                  <option>Senior</option>
                  <option>Manager</option>
                  <option>Not sure</option>
                </select>
              </label>

              <label className="wideField">
                What is your education level or current student status?
                <input
                  value={profile.educationStatus}
                  onChange={(event) => updateProfile('educationStatus', event.target.value)}
                  placeholder="Current student, recent graduate, bachelor's degree, bootcamp..."
                />
              </label>

              <div className="wideField resumeBox">
                <div>
                  <FileText size={20} />
                  <div>
                    <strong>{resume ? resume.name : 'Upload your resume'}</strong>
                    <span>
                      {resume
                        ? `${formatFileSize(resume.size)} saved for this browser`
                        : 'PDF, DOC, or DOCX. This is saved locally so refreshes do not wipe it.'}
                    </span>
                  </div>
                </div>
                <div className="resumeActions">
                  <input
                    ref={fileInputRef}
                    type="file"
                    accept=".pdf,.doc,.docx,application/pdf,application/msword,application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                    onChange={handleResumeUpload}
                    aria-label="Upload resume"
                  />
                  <button type="button" onClick={() => fileInputRef.current?.click()}>
                    <Upload size={18} />
                    {resume ? 'Replace' : 'Upload'}
                  </button>
                  {resume && (
                    <button className="ghostButton" type="button" onClick={removeResume}>
                      <Trash2 size={18} />
                      Remove
                    </button>
                  )}
                </div>
              </div>
            </div>
          </section>
          )}

          {activeTab === 'details' && (
          <section className="formSection" aria-labelledby="personal-heading">
            <div className="sectionHeader">
              <div>
                <span>Step 3</span>
                <h2 id="personal-heading">Personal search details</h2>
              </div>
            </div>
            <label>
              Describe the tailored things you want this search to care about.
              <textarea
                value={profile.personalDescription}
                onChange={(event) => updateProfile('personalDescription', event.target.value)}
                placeholder="Skills you want to gain, team environment, values, mentorship, pace, problems you want to work on..."
                rows={8}
              />
            </label>
          </section>
          )}

          <div className="stepControls" aria-label="Search setup navigation">
            <button
              type="button"
              className="secondaryAction"
              disabled={activeTab === 'sources'}
              onClick={() => setActiveTab(activeTab === 'details' ? 'questionnaire' : 'sources')}
            >
              <ArrowLeft size={18} />
              Back
            </button>
            {activeTab !== 'details' ? (
              <button
                type="button"
                className="primaryAction inlineAction"
                onClick={() => setActiveTab(activeTab === 'sources' ? 'questionnaire' : 'details')}
              >
                Continue
                <ArrowRight size={18} />
              </button>
            ) : (
              <button
                className="primaryAction inlineAction"
                type="button"
                disabled={!readyToSearch || searchState === 'searching'}
                onClick={saveSearchProfile}
              >
                {searchState === 'searching' ? <Loader2 className="spinIcon" size={18} /> : <Search size={18} />}
                {searchState === 'searching' ? 'Searching job sites' : 'Save Search Profile'}
              </button>
            )}
          </div>
        </form>

        <aside className="summaryPanel" aria-label="Search profile summary">
          <div className="summaryHeader">
            <span>Search profile</span>
            <strong>{readyToSearch ? 'Ready' : 'In progress'}</strong>
          </div>
          <div className="progressTrack" aria-label={`${completion}% complete`}>
            <span style={{ width: `${completion}%` }} />
          </div>

          <dl className="summaryList">
            <SummaryItem label="Sources" value={sources.map((source) => source.label).join(', ')} />
            <SummaryItem label="Opportunity" value={profile.opportunityType} />
            <SummaryItem label="Work style" value={profile.workStyle} />
            <SummaryItem label="Location" value={profile.location} />
            <SummaryItem label="Fields" value={profile.industries} />
            <SummaryItem label="Roles" value={profile.roles} />
            <SummaryItem label="Experience" value={profile.experienceLevel} />
            <SummaryItem label="Education" value={profile.educationStatus} />
            <SummaryItem label="Resume" value={resume?.name} />
          </dl>

          <div className="searchBrief">
            <h3>Tailored description</h3>
            <p>
              {profile.personalDescription ||
                'Add what makes your search unique so future matching can prioritize fit, growth, and work environment.'}
            </p>
          </div>

          {searchSavedAt && <p className="savedProfileNote">Search profile saved at {searchSavedAt}.</p>}

          <button
            className="primaryAction"
            type="button"
            disabled={!readyToSearch || searchState === 'searching'}
            onClick={saveSearchProfile}
          >
            {searchState === 'searching' ? <Loader2 className="spinIcon" size={18} /> : <Search size={18} />}
            {searchState === 'searching' ? 'Searching job sites' : 'Save Search Profile'}
          </button>
        </aside>
      </section>
      )}

      {activeTab === 'listings' && (
        <section className="resultsPanel listingsPage" aria-live="polite" aria-labelledby="results-heading">
          <div className="sectionHeader">
            <div>
              <span>Search results</span>
              <h2 id="results-heading">Matched listings from the last two weeks</h2>
            </div>
            <strong>
              {searchState === 'searching'
                ? 'Searching'
                : `${jobMatches.length} match${jobMatches.length === 1 ? '' : 'es'}`}
            </strong>
          </div>

          {searchState === 'idle' && jobMatches.length === 0 && (
            <div className="listingEmptyHero">
              <div>
                <Search size={28} />
              </div>
              <h3>No listings searched yet</h3>
              <p>Finish the setup pages and save the search profile to pull matching opportunities here.</p>
              <button type="button" className="primaryAction inlineAction" onClick={() => setActiveTab('details')}>
                Go to personal details
                <ArrowRight size={18} />
              </button>
            </div>
          )}

          {searchState === 'searching' && (
            <div className="searchingState">
              <Loader2 className="spinIcon" size={22} />
              <div>
                <strong>Checking selected job sites and reading job descriptions.</strong>
                <span>Some sources may block automated reading; those will be listed in the source notes.</span>
              </div>
            </div>
          )}

          {searchError && <p className="errorNote">{searchError}</p>}

          {searchState === 'done' && jobMatches.length === 0 && (
            <p className="emptyResults">
              No strong matches came back from readable listings. Try adding more specific role titles,
              a company career page, or a Greenhouse/Lever board URL.
            </p>
          )}

          {jobMatches.length > 0 && (
            <div className="resultsScroller">
              {jobMatches.map((match) => {
                const isSaved = savedOpportunities.some(
                  (item) => item.id === match.id || item.url === match.url,
                );
                return (
                  <article className="jobCard" key={match.id}>
                    <div className="jobCardTop">
                      <div>
                        <span>{match.source}</span>
                        <h3>{match.title}</h3>
                        <p>{match.company}</p>
                      </div>
                      <div className="jobScoreActions">
                        <strong>{match.matchPercent}%</strong>
                        <button type="button" onClick={() => saveOpportunity(match)} disabled={isSaved}>
                          <Bookmark size={16} />
                          {isSaved ? 'Saved' : 'Save'}
                        </button>
                      </div>
                    </div>
                    <div className="jobMeta">
                      <span>{match.location}</span>
                      <span>{match.recency}</span>
                    </div>
                    <p className="jobSnippet">{match.snippet}</p>
                    <ul className="matchReasons">
                      {match.reasons.map((reason) => (
                        <li key={reason}>{reason}</li>
                      ))}
                    </ul>
                    <a className="applyLink" href={match.url} target="_blank" rel="noreferrer">
                      <ExternalLink size={16} />
                      Open listing
                    </a>
                  </article>
                );
              })}
            </div>
          )}

          {sourceNotes.length > 0 && (
            <div className="sourceNotes">
              {sourceNotes.map((note) => (
                <div key={`${note.source}-${note.status}`}>
                  <strong>{note.source}</strong>
                  <span>{note.message}</span>
                </div>
              ))}
            </div>
          )}
        </section>
      )}

      {activeTab === 'profile' && (
        <ProfileDashboard
          applications={applications}
          profile={profile}
          removeApplication={removeApplication}
          removeSavedOpportunity={removeSavedOpportunity}
          savedOpportunities={savedOpportunities}
          updateApplication={updateApplication}
          updateProfile={updateProfile}
          addApplication={addApplication}
        />
      )}
    </main>
  );
}

function SummaryItem({ label, value }) {
  return (
    <div>
      <dt>{label}</dt>
      <dd>{value || 'Not added yet'}</dd>
    </div>
  );
}

function ProfileDashboard({
  addApplication,
  applications,
  profile,
  removeApplication,
  removeSavedOpportunity,
  savedOpportunities,
  updateApplication,
  updateProfile,
}) {
  return (
    <section className="profileDashboard" aria-labelledby="profile-heading">
      <div className="dashboardHeader">
        <div>
          <span>Profile dashboard</span>
          <h2 id="profile-heading">Your search profile and opportunity pipeline</h2>
        </div>
        <div className="dashboardStats" aria-label="Profile stats">
          <strong>{savedOpportunities.length} saved</strong>
          <strong>{applications.length} tracked</strong>
        </div>
      </div>

      <section className="dashboardSection" aria-labelledby="profile-editor-heading">
        <div className="sectionHeader compactHeader">
          <div>
            <span>Edit profile</span>
            <h2 id="profile-editor-heading">What you are looking for</h2>
          </div>
          <LayoutDashboard size={22} />
        </div>

        <div className="profileEditorGrid">
          <label>
            Opportunity type
            <select
              value={profile.opportunityType}
              onChange={(event) => updateProfile('opportunityType', event.target.value)}
            >
              <option>Full-time</option>
              <option>Part-time</option>
              <option>Internship</option>
              <option>Contract</option>
              <option>Fellowship</option>
              <option>Not sure</option>
            </select>
          </label>

          <label>
            Work style
            <select
              value={profile.workStyle}
              onChange={(event) => updateProfile('workStyle', event.target.value)}
            >
              <option>Remote</option>
              <option>Hybrid</option>
              <option>In-person</option>
              <option>Flexible / open to any</option>
            </select>
          </label>

          <ProfileTextField label="Location" field="location" profile={profile} updateProfile={updateProfile} />
          <ProfileTextField label="Fields" field="industries" profile={profile} updateProfile={updateProfile} />
          <ProfileTextField label="Roles" field="roles" profile={profile} updateProfile={updateProfile} />

          <label>
            Experience level
            <select
              value={profile.experienceLevel}
              onChange={(event) => updateProfile('experienceLevel', event.target.value)}
            >
              <option>Entry-level</option>
              <option>Internship / student</option>
              <option>Associate</option>
              <option>Mid-level</option>
              <option>Senior</option>
              <option>Manager</option>
              <option>Not sure</option>
            </select>
          </label>

          <ProfileTextField
            label="Education"
            field="educationStatus"
            profile={profile}
            updateProfile={updateProfile}
          />

          <label className="wideField">
            Tailored description
            <textarea
              value={profile.personalDescription}
              onChange={(event) => updateProfile('personalDescription', event.target.value)}
              rows={6}
            />
          </label>
        </div>
      </section>

      <section className="dashboardSection" aria-labelledby="saved-heading">
        <div className="sectionHeader compactHeader">
          <div>
            <span>Saved opportunities</span>
            <h2 id="saved-heading">Jobs to review</h2>
          </div>
          <Bookmark size={22} />
        </div>

        {savedOpportunities.length === 0 ? (
          <p className="emptyResults">Saved jobs will show up here when you save a match from search results.</p>
        ) : (
          <div className="savedGrid">
            {savedOpportunities.map((opportunity) => (
              <article className="savedOpportunity" key={opportunity.id}>
                <div>
                  <span>{opportunity.source}</span>
                  <h3>{opportunity.title}</h3>
                  <p>{opportunity.company}</p>
                </div>
                <div className="jobMeta">
                  <span>{opportunity.matchPercent}% match</span>
                  <span>{opportunity.location}</span>
                </div>
                <div className="savedActions">
                  <a href={opportunity.url} target="_blank" rel="noreferrer">
                    <ExternalLink size={16} />
                    Open
                  </a>
                  <button type="button" onClick={() => addApplication(opportunity)}>
                    <ClipboardList size={16} />
                    Track
                  </button>
                  <button type="button" onClick={() => removeSavedOpportunity(opportunity.id)}>
                    <Trash2 size={16} />
                    Remove
                  </button>
                </div>
              </article>
            ))}
          </div>
        )}
      </section>

      <section className="dashboardSection" aria-labelledby="tracker-heading">
        <div className="sectionHeader compactHeader">
          <div>
            <span>Application tracker</span>
            <h2 id="tracker-heading">Spreadsheet deck</h2>
          </div>
          <ClipboardList size={22} />
        </div>

        {applications.length === 0 ? (
          <p className="emptyResults">Track saved opportunities to build your application spreadsheet.</p>
        ) : (
          <div className="trackerTableWrap">
            <table className="trackerTable">
              <thead>
                <tr>
                  <th>Role</th>
                  <th>Company</th>
                  <th>Match</th>
                  <th>Status</th>
                  <th>Applied</th>
                  <th>Follow up</th>
                  <th>Contact</th>
                  <th>Notes</th>
                  <th>Link</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                {applications.map((application) => (
                  <tr key={application.id}>
                    <td>
                      <strong>{application.title}</strong>
                      <span>{application.source}</span>
                    </td>
                    <td>{application.company}</td>
                    <td>{application.matchPercent}%</td>
                    <td>
                      <select
                        value={application.status}
                        onChange={(event) => updateApplication(application.id, 'status', event.target.value)}
                      >
                        <option>Saved</option>
                        <option>Applied</option>
                        <option>Interviewing</option>
                        <option>Followed up</option>
                        <option>Rejected</option>
                        <option>Offer</option>
                      </select>
                    </td>
                    <td>
                      <input
                        type="date"
                        value={application.appliedDate}
                        onChange={(event) => updateApplication(application.id, 'appliedDate', event.target.value)}
                      />
                    </td>
                    <td>
                      <input
                        type="date"
                        value={application.followUpDate}
                        onChange={(event) => updateApplication(application.id, 'followUpDate', event.target.value)}
                      />
                    </td>
                    <td>
                      <input
                        value={application.contact}
                        onChange={(event) => updateApplication(application.id, 'contact', event.target.value)}
                        placeholder="Name or email"
                      />
                    </td>
                    <td>
                      <input
                        value={application.notes}
                        onChange={(event) => updateApplication(application.id, 'notes', event.target.value)}
                        placeholder="Next step"
                      />
                    </td>
                    <td>
                      <a href={application.url} target="_blank" rel="noreferrer">
                        <ExternalLink size={16} />
                      </a>
                    </td>
                    <td>
                      <button
                        aria-label={`Remove ${application.title}`}
                        type="button"
                        onClick={() => removeApplication(application.id)}
                      >
                        <X size={16} />
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </section>
  );
}

function ProfileTextField({ field, label, profile, updateProfile }) {
  return (
    <label>
      {label}
      <input value={profile[field]} onChange={(event) => updateProfile(field, event.target.value)} />
    </label>
  );
}

function usePersistentState(key, defaultValue) {
  const [value, setValue] = useState(() => {
    try {
      const stored = window.localStorage.getItem(STORAGE_KEY);
      if (!stored) return defaultValue;
      const parsed = JSON.parse(stored);
      return parsed[key] ?? defaultValue;
    } catch {
      return defaultValue;
    }
  });

  useEffect(() => {
    try {
      const stored = window.localStorage.getItem(STORAGE_KEY);
      const parsed = stored ? JSON.parse(stored) : {};
      window.localStorage.setItem(STORAGE_KEY, JSON.stringify({ ...parsed, [key]: value }));
    } catch {
      // Auto-save should never block form entry.
    }
  }, [key, value]);

  return [value, setValue];
}

function normalizeSource(value) {
  const trimmed = value.trim();
  if (!trimmed) return null;
  const hasProtocol = /^https?:\/\//i.test(trimmed);
  const url = hasProtocol ? trimmed : `https://${trimmed}`;
  let label = trimmed.replace(/^https?:\/\//i, '').replace(/^www\./i, '').split('/')[0];
  label = label || trimmed;
  return {
    id: `source-${Date.now()}-${Math.random().toString(16).slice(2)}`,
    label,
    url,
  };
}

function formatSavedAt(date) {
  return date.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
}

function formatFileSize(bytes = 0) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function openResumeDb() {
  return new Promise((resolve, reject) => {
    const request = window.indexedDB.open(RESUME_DB, 1);
    request.onupgradeneeded = () => {
      request.result.createObjectStore(RESUME_STORE);
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function loadResume() {
  if (!window.indexedDB) return null;
  try {
    const db = await openResumeDb();
    return await new Promise((resolve, reject) => {
      const request = db.transaction(RESUME_STORE, 'readonly').objectStore(RESUME_STORE).get(RESUME_KEY);
      request.onsuccess = () => resolve(request.result ?? null);
      request.onerror = () => reject(request.error);
    });
  } catch {
    return null;
  }
}

async function saveResume(resume) {
  if (!window.indexedDB) return;
  const db = await openResumeDb();
  await new Promise((resolve, reject) => {
    const request = db.transaction(RESUME_STORE, 'readwrite').objectStore(RESUME_STORE).put(resume, RESUME_KEY);
    request.onsuccess = () => resolve();
    request.onerror = () => reject(request.error);
  });
}

async function deleteResume() {
  if (!window.indexedDB) return;
  const db = await openResumeDb();
  await new Promise((resolve, reject) => {
    const request = db.transaction(RESUME_STORE, 'readwrite').objectStore(RESUME_STORE).delete(RESUME_KEY);
    request.onsuccess = () => resolve();
    request.onerror = () => reject(request.error);
  });
}

createRoot(document.getElementById('root')).render(<App />);
